// Copyright 2024 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{cmp, sync::Arc};

use anyhow::Context;
use arrow_schema::Schema;
use config::{
    get_config, is_local_disk_storage,
    meta::{
        bitvec::BitVec,
        puffin_directory::reader::RamDirectoryReader,
        search::{ScanStats, StorageType},
        stream::FileKey,
    },
    utils::inverted_index::convert_parquet_idx_file_name_to_tantivy_file,
};
use datafusion::execution::cache::cache_manager::FileStatisticsCache;
use futures::{future::try_join_all, io::Cursor};
use hashbrown::{HashMap, HashSet};
use infra::{
    cache::file_data,
    errors::{Error, ErrorCodes},
    schema::get_stream_setting_index_fields,
};
use itertools::Itertools;
use proto::cluster_rpc::KvItem;
use tantivy::{
    query::{PhrasePrefixQuery, Query, QueryParser, RegexQuery},
    Directory, Term,
};
use tokio::sync::Semaphore;
use tracing::Instrument;

use crate::service::{
    db, file_list,
    search::{datafusion::exec, generate_filter_from_equal_items, generate_search_schema_diff},
};

type CachedFiles = (usize, usize);

/// search in remote object storage
#[tracing::instrument(name = "service:search:grpc:storage", skip_all, fields(org_id = query.org_id, stream_name = query.stream_name))]
pub async fn search(
    query: Arc<super::QueryParams>,
    schema: Arc<Schema>,
    file_list: &[FileKey],
    req_equal_terms: &[KvItem],
    req_match_terms: &[String],
    sorted_by_time: bool,
    file_stat_cache: Option<FileStatisticsCache>,
) -> super::SearchTable {
    let enter_span = tracing::span::Span::current();
    log::info!("[trace_id {}] search->storage: enter", query.trace_id);
    // fetch all schema versions, group files by version
    let schema_versions = match infra::schema::get_versions(
        &query.org_id,
        &query.stream_name,
        query.stream_type,
        query.time_range,
    )
    .instrument(enter_span.clone())
    .await
    {
        Ok(versions) => versions,
        Err(err) => {
            log::error!("[trace_id {}] get schema error: {}", query.trace_id, err);
            return Err(Error::ErrorCode(ErrorCodes::SearchStreamNotFound(
                query.stream_name.clone(),
            )));
        }
    };
    log::info!(
        "[trace_id {}] search->storage: stream {}/{}/{}, get schema versions num {}",
        query.trace_id,
        query.org_id,
        query.stream_type,
        query.stream_name,
        schema_versions.len()
    );
    if schema_versions.is_empty() {
        return Ok((vec![], ScanStats::new()));
    }
    let schema_latest_id = schema_versions.len() - 1;

    // get file list
    let mut files = file_list.to_vec();
    if files.is_empty() {
        return Ok((vec![], ScanStats::default()));
    }
    log::info!(
        "[trace_id {}] search->storage: stream {}/{}/{}, load file_list num {}",
        query.trace_id,
        query.org_id,
        query.stream_type,
        query.stream_name,
        files.len(),
    );

    // check inverted index
    let cfg = get_config();
    let inverted_index_type = cfg.common.inverted_index_search_format.clone();
    let use_inverted_index = query.use_inverted_index && inverted_index_type == "tantivy";
    if use_inverted_index {
        log::info!(
            "[trace_id {}] flight->search: use_inverted_index with tantivy format {}",
            query.trace_id,
            use_inverted_index
        );
    }

    let mut idx_took = 0;
    if use_inverted_index {
        idx_took = filter_file_list_by_tantivy_index(
            query.clone(),
            &mut files,
            req_equal_terms,
            req_match_terms,
        )
        .await?;
        log::info!(
            "[trace_id {}] search->storage: stream {}/{}/{}, {} inverted index reduced file_list num to {} in {} ms",
            query.trace_id,
            query.org_id,
            query.stream_type,
            query.stream_name,
            inverted_index_type,
            files.len(),
            idx_took
        );
    }

    let cfg = get_config();
    let mut files_group: HashMap<usize, Vec<FileKey>> =
        HashMap::with_capacity(schema_versions.len());
    let mut scan_stats = ScanStats::new();
    if !cfg.common.widening_schema_evolution || schema_versions.len() == 1 {
        let files = files.to_vec();
        scan_stats = match file_list::calculate_files_size(&files).await {
            Ok(size) => size,
            Err(err) => {
                log::error!(
                    "[trace_id {}] calculate files size error: {}",
                    query.trace_id,
                    err
                );
                return Err(Error::ErrorCode(ErrorCodes::ServerInternalError(
                    "calculate files size error".to_string(),
                )));
            }
        };
        files_group.insert(schema_latest_id, files);
    } else {
        scan_stats.files = files.len() as i64;
        for file in files.iter() {
            // calculate scan size
            scan_stats.records += file.meta.records;
            scan_stats.original_size += file.meta.original_size;
            scan_stats.compressed_size += file.meta.compressed_size;
            // check schema version
            let schema_ver_id = match db::schema::filter_schema_version_id(
                &schema_versions,
                file.meta.min_ts,
                file.meta.max_ts,
            ) {
                Some(id) => id,
                None => {
                    log::error!(
                        "[trace_id {}] search->storage: file {} schema version not found, will use the latest schema, min_ts: {}, max_ts: {}",
                        query.trace_id,
                        &file.key,
                        file.meta.min_ts,
                        file.meta.max_ts
                    );
                    // HACK: use the latest version if not found in schema versions
                    schema_latest_id
                }
            };
            let group = files_group.entry(schema_ver_id).or_default();
            group.push(file.clone());
        }
    }

    log::info!(
        "[trace_id {}] search->storage: stream {}/{}/{}, load files {}, scan_size {}, compressed_size {}",
        query.trace_id,
        query.org_id,
        query.stream_type,
        query.stream_name,
        scan_stats.files,
        scan_stats.original_size,
        scan_stats.compressed_size
    );

    if cfg.common.memory_circuit_breaker_enable {
        super::check_memory_circuit_breaker(&query.trace_id, &scan_stats)?;
    }

    // load files to local cache
    let cache_start = std::time::Instant::now();
    let (cache_type, deleted_files, (mem_cached_files, disk_cached_files)) = cache_files(
        &query.trace_id,
        &files.iter().map(|f| f.key.as_ref()).collect_vec(),
        &scan_stats,
    )
    .instrument(enter_span.clone())
    .await?;
    if !deleted_files.is_empty() {
        // remove deleted files from files_group
        for (_, g_files) in files_group.iter_mut() {
            g_files.retain(|f| !deleted_files.contains(&f.key));
        }
    }

    scan_stats.idx_took = idx_took as i64;
    scan_stats.querier_files = scan_stats.files;
    scan_stats.querier_memory_cached_files = mem_cached_files as i64;
    scan_stats.querier_disk_cached_files = disk_cached_files as i64;
    log::info!(
        "[trace_id {}] search->storage: stream {}/{}/{}, load files {}, memory cached {}, disk cached {}, download others into {:?} cache done, took: {} ms",
        query.trace_id,
        query.org_id,
        query.stream_type,
        query.stream_name,
        scan_stats.querier_files,
        scan_stats.querier_memory_cached_files,
        scan_stats.querier_disk_cached_files,
        cache_type,
        cache_start.elapsed().as_millis()
    );

    // set target partitions based on cache type
    let target_partitions = if cache_type == file_data::CacheType::None {
        cfg.limit.query_thread_num
    } else {
        cfg.limit.cpu_num
    };

    // construct latest schema map
    let schema_latest = Arc::new(
        schema
            .as_ref()
            .clone()
            .with_metadata(std::collections::HashMap::new()),
    );
    let mut schema_latest_map = HashMap::with_capacity(schema_latest.fields().len());
    for field in schema_latest.fields() {
        schema_latest_map.insert(field.name(), field);
    }

    let mut tables = Vec::new();
    for (ver, files) in files_group {
        if files.is_empty() {
            continue;
        }
        if files.is_empty() {
            continue;
        }
        let schema = schema_versions[ver].clone();
        let schema = schema.with_metadata(std::collections::HashMap::new());

        let session = config::meta::search::Session {
            id: format!("{}-{ver}", query.trace_id),
            storage_type: StorageType::Memory,
            work_group: query.work_group.clone(),
            target_partitions,
        };

        let diff_fields = generate_search_schema_diff(&schema, &schema_latest_map)?;
        let table = exec::create_parquet_table(
            &session,
            schema_latest.clone(),
            &files,
            diff_fields,
            sorted_by_time,
            file_stat_cache.clone(),
        )
        .await?;
        tables.push(table);
    }

    Ok((tables, scan_stats))
}

#[tracing::instrument(name = "service:search:grpc:storage:cache_files", skip_all)]
async fn cache_files(
    trace_id: &str,
    files: &[&str],
    scan_stats: &ScanStats,
) -> Result<(file_data::CacheType, Vec<String>, CachedFiles), Error> {
    let cfg = get_config();
    let cache_type = if cfg.memory_cache.enabled
        && scan_stats.compressed_size < cfg.memory_cache.skip_size as i64
    {
        // if scan_compressed_size < 80% of total memory cache, use memory cache
        file_data::CacheType::Memory
    } else if !is_local_disk_storage()
        && cfg.disk_cache.enabled
        && scan_stats.compressed_size < cfg.disk_cache.skip_size as i64
    {
        // if scan_compressed_size < 80% of total disk cache, use disk cache
        file_data::CacheType::Disk
    } else {
        // no cache
        return Ok((file_data::CacheType::None, vec![], (0, 0)));
    };

    let mut mem_cached_files = 0;
    let mut disk_cached_files = 0;

    let mut tasks = Vec::new();
    let semaphore = std::sync::Arc::new(Semaphore::new(cfg.limit.query_thread_num));
    for file in files.iter() {
        let trace_id = trace_id.to_string();
        let file_name = file.to_string();
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let task: tokio::task::JoinHandle<(Option<String>, bool, bool)> =
            tokio::task::spawn(async move {
                let cfg = get_config();
                let ret = match cache_type {
                    file_data::CacheType::Memory => {
                        let mut disk_exists = false;
                        let mem_exists = file_data::memory::exist(&file_name).await;
                        if !mem_exists && !cfg.memory_cache.skip_disk_check {
                            // when skip_disk_check = false, need to check disk cache
                            disk_exists = file_data::disk::exist(&file_name).await;
                        }
                        if !mem_exists && (cfg.memory_cache.skip_disk_check || !disk_exists) {
                            (
                                file_data::memory::download(&trace_id, &file_name)
                                    .await
                                    .err(),
                                false,
                                false,
                            )
                        } else {
                            (None, mem_exists, disk_exists)
                        }
                    }
                    file_data::CacheType::Disk => {
                        if !file_data::disk::exist(&file_name).await {
                            (
                                file_data::disk::download(&trace_id, &file_name).await.err(),
                                false,
                                false,
                            )
                        } else {
                            (None, false, true)
                        }
                    }
                    _ => (None, false, false),
                };
                // return file_name if download failed
                let file_name = if let Some(e) = ret.0 {
                    log::warn!(
                        "[trace_id {trace_id}] search->storage: download file to cache err: {}",
                        e
                    );
                    Some(file_name)
                } else {
                    None
                };
                drop(permit);
                (file_name, ret.1, ret.2)
            });
        tasks.push(task);
    }

    let mut delete_files = Vec::new();
    for task in tasks {
        match task.await {
            Ok((file, mem_exists, disk_exists)) => {
                if mem_exists {
                    mem_cached_files += 1;
                } else if disk_exists {
                    disk_cached_files += 1;
                }
                if let Some(file) = file {
                    delete_files.push(file);
                }
            }
            Err(e) => {
                log::error!(
                    "[trace_id {trace_id}] search->storage: load file task err: {}",
                    e
                );
            }
        }
    }

    Ok((
        cache_type,
        delete_files,
        (mem_cached_files, disk_cached_files),
    ))
}

/// Filter file list using inverted index
/// This function will load the index file corresponding to each file in the file list.
/// FSTs in those files are used to match the incoming query in `SearchRequest`.
/// If the query does not match any FST in the index file, the file will be filtered out.
/// If the query does match then the segment IDs for the file will be updated.
/// If the query not find corresponding index file, the file will *not* be filtered out.
async fn filter_file_list_by_tantivy_index(
    query: Arc<super::QueryParams>,
    file_list: &mut Vec<FileKey>,
    equal_terms: &[KvItem],
    match_terms: &[String],
) -> Result<usize, Error> {
    let start = std::time::Instant::now();
    let cfg = get_config();

    // construct partition filters
    let equal_terms: Vec<(String, String)> = equal_terms
        .iter()
        .map(|v| (v.key.to_string(), v.value.to_string()))
        .collect::<Vec<_>>();
    // filter equal_items with index_fields
    let schema = infra::schema::get(&query.org_id, &query.stream_name, query.stream_type).await?;
    let stream_settings = infra::schema::unwrap_stream_settings(&schema);
    let index_fields = get_stream_setting_index_fields(&stream_settings);
    let index_terms_orig = super::super::filter_index_fields(&equal_terms, &index_fields);
    let index_terms = generate_filter_from_equal_items(&index_terms_orig);

    // Cache the corresponding Index files
    let mut scan_stats = ScanStats::new();
    let mut file_list_map = file_list
        .drain(..)
        .map(|f| (f.key.clone(), f))
        .collect::<HashMap<_, _>>();
    let index_file_names = file_list_map
        .iter()
        .filter_map(|(_, f)| {
            if f.meta.index_size > 0 {
                convert_parquet_idx_file_name_to_tantivy_file(&f.key).map(|v| (f.key.clone(), v))
            } else {
                None
            }
        })
        .collect_vec();
    let (cache_type, _, (mem_cached_files, disk_cached_files)) = cache_files(
        &query.trace_id,
        &index_file_names
            .iter()
            .map(|(_parquet_file, ttv_file)| ttv_file.as_str())
            .collect_vec(),
        &scan_stats,
    )
    .await?;

    scan_stats.querier_memory_cached_files = mem_cached_files as i64;
    scan_stats.querier_disk_cached_files = disk_cached_files as i64;
    log::info!(
        "[trace_id {}] search->storage: stream {}/{}/{}, load puffin index files {}, memory cached {}, disk cached {}, download others into {:?} cache done, took: {} ms",
        query.trace_id,
        query.org_id,
        query.stream_type,
        query.stream_name,
        scan_stats.querier_files,
        scan_stats.querier_memory_cached_files,
        scan_stats.querier_disk_cached_files,
        cache_type,
        start.elapsed().as_millis()
    );

    let full_text_terms = Arc::new(match_terms.to_vec());
    let index_terms = Arc::new(index_terms);

    let semaphore = std::sync::Arc::new(Semaphore::new(cfg.limit.cpu_num));
    let mut tasks = Vec::new();

    for (parquet_file, _ttv_file) in index_file_names {
        let file = parquet_file;
        let full_text_term_clone = full_text_terms.clone();
        let index_terms_clone = index_terms.clone();
        let trace_id = query.trace_id.to_string();
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        // Spawn a task for each file, wherein full text search and
        // secondary index search queries are executed
        let task = tokio::task::spawn(async move {
            let res =
                search_tantivy_index(&trace_id, &file, full_text_term_clone, index_terms_clone)
                    .await;
            drop(permit);
            res
        });
        tasks.push(task)
    }

    let mut total_hits = 0;
    for result in try_join_all(tasks)
        .await
        .map_err(|e| Error::ErrorCode(ErrorCodes::ServerInternalError(e.to_string())))?
    {
        let result: anyhow::Result<(String, Option<BitVec>)> = result;
        // Each result corresponds to a file in the file list
        match result {
            Ok((file_name, bitvec)) => {
                if let Some(res) = bitvec {
                    // Replace the segment IDs in the existing `FileKey` with the new found segments
                    let file = file_list_map
                        .get_mut(&file_name)
                        // we expect each file name has atleast 1 file
                        .unwrap();
                    let hits_per_file = res.count_ones();
                    total_hits += hits_per_file;
                    file.segment_ids = Some(res.into_vec());
                    log::debug!(
                        "[trace_id {}] search->storage: hits for fts_terms {:?} and index_terms: {:?} found {} in {}",
                        query.trace_id,
                        *full_text_terms,
                        index_terms,
                        hits_per_file,
                        file_name
                    );
                } else {
                    // if the bitmap is empty then we remove the file from the list
                    log::debug!(
                        "[trace_id {}] search->storage: no match found in index for file {}",
                        query.trace_id,
                        file_name
                    );
                    file_list_map.remove(&file_name);
                }
            }
            Err(e) => {
                log::warn!(
                    "[trace_id {}] search->storage: error filtering file via FST index. Keep file to search. error: {}",
                    query.trace_id,
                    e.to_string()
                );
                continue;
            }
        }
    }
    log::info!(
        "[trace_id {}] search->storage: Total hits for fts_terms {:?} and index_terms: {:?} found {}",
        query.trace_id,
        *full_text_terms,
        index_terms,
        total_hits
    );
    file_list.extend(file_list_map.into_values());
    Ok(start.elapsed().as_millis() as usize)
}

fn search_tantivy_index_with_field(
    query_parser: QueryParser,
    field: tantivy::schema::Field,
    tantivy_searcher: tantivy::Searcher,
    fts_terms: &[String],
) -> anyhow::Result<(HashSet<tantivy::DocAddress>, u32)> {
    let cfg = get_config();
    let mut max_doc_id = 0;
    let mut matched_docs = HashSet::new();

    // TODO: improve this logic
    // The issue is joining the full text search terms
    // Ideally there should be multiple queries for each term
    // and their result should be intersected
    let query: Box<dyn Query> = match cfg.common.full_text_search_type.as_str() {
        "eq" => query_parser.parse_query(&fts_terms.join(" "))?,
        "contains" => Box::new(RegexQuery::from_pattern(
            &format!("*{}*", &fts_terms.join(" ")),
            field,
        )?),
        // Default to prefix search
        _ => Box::new(PhrasePrefixQuery::new(vec![Term::from_field_text(
            field,
            // we join all the fields that are full text searchable
            &fts_terms.join(" "),
        )])),
    };

    let docs = tantivy_searcher.search(&query, &tantivy::collector::DocSetCollector)?;
    for doc in docs {
        max_doc_id = cmp::max(doc.doc_id, max_doc_id);
        matched_docs.insert(doc);
    }

    Ok((matched_docs, max_doc_id))
}

pub async fn get_tantivy_directory(
    trace_id: &str,
    file_name: &str,
) -> anyhow::Result<Box<dyn Directory>> {
    let index_file_bytes = infra::cache::file_data::get(trace_id, file_name, None).await?;
    let puffin_dir = RamDirectoryReader::from_bytes(Cursor::new(index_file_bytes)).await?;
    Ok(Box::new(puffin_dir))
}

async fn search_tantivy_index(
    trace_id: &str,
    parquet_file_name: &str,
    fts_terms: Arc<Vec<String>>,
    index_terms: Arc<Vec<(String, Vec<String>)>>,
) -> anyhow::Result<(String, Option<BitVec>)> {
    let Some(ttv_file_name) = convert_parquet_idx_file_name_to_tantivy_file(parquet_file_name)
    else {
        return Err(anyhow::anyhow!(
            "[trace_id {trace_id}] search->storage: Unable to find tantivy index files for parquet file {}",
            parquet_file_name
        ));
    };

    let puffin_dir = get_tantivy_directory(trace_id, &ttv_file_name).await?;
    let tantivy_index = tantivy::Index::open(puffin_dir)?;
    let tantivy_schema = tantivy_index.schema();
    let tantivy_reader = tantivy_index.reader()?;
    let tantivy_searcher = tantivy_reader.searcher();

    let mut matched_docs = HashSet::new();
    let mut max_doc_id = 0u32;
    if !fts_terms.is_empty() {
        let fts_field = tantivy_schema.get_field("_all").unwrap();
        let query_parser = QueryParser::for_index(&tantivy_index, vec![fts_field]);
        let tantivy_searcher_clone = tantivy_searcher.clone();
        if let Ok((docs, max_row)) = tokio::task::spawn_blocking(move || {
            search_tantivy_index_with_field(
                query_parser,
                fts_field,
                tantivy_searcher_clone,
                &fts_terms,
            )
        })
        .await?
        {
            matched_docs.extend(docs);
            max_doc_id = max_row;
        } else {
            return Err(anyhow::anyhow!(
                "[trace_id {}] search->storage: search_full_text_index failed",
                trace_id
            ));
        };
    }
    for (field_name, terms) in index_terms.iter() {
        let field = tantivy_schema.get_field(field_name).unwrap();
        let query_parser = QueryParser::for_index(&tantivy_index, vec![field]);
        let tantivy_searcher_clone = tantivy_searcher.clone();
        let terms = terms.clone();

        if let Ok((docs, max_row)) = tokio::task::spawn_blocking(move || {
            search_tantivy_index_with_field(query_parser, field, tantivy_searcher_clone, &terms)
        })
        .await?
        {
            matched_docs.extend(docs);
            max_doc_id = max_row;
        } else {
            return Err(anyhow::anyhow!(
                "[trace_id {}] search->storage: search_full_text_index failed",
                trace_id
            ));
        };
    }

    // return early if no matches in tantivy
    if matched_docs.is_empty() {
        return Ok((parquet_file_name.to_string(), None));
    }

    // Prepare a vec of segment offsets
    // this is useful when there are more than one segments
    let seg_metas = tantivy_index
        .searchable_segment_metas()
        .context("Count segments")?;
    let mut res = BitVec::with_capacity(max_doc_id as usize + 1);
    // rewrite the empty bitvec
    res.resize((max_doc_id + 1) as usize, false);
    // Return early if there is only one segment
    if seg_metas.len() == 1 {
        for doc in matched_docs {
            res.set(doc.doc_id as usize, true);
        }
        Ok((parquet_file_name.to_string(), Some(res)))
    } else {
        Err(anyhow::anyhow!(
            "[trace_id {trace_id}] search->storage: Multiple segments in tantivy index not supported"
        ))
    }
}
