//! Retained per-source snapshots with generation-based revalidation.
//!
//! Queries clone snapshot `Arc`s under a short store lock and run lock-free.
//! TTL-fresh entries serve outright; stale ones revalidate against file state
//! (reused without decode when unchanged); only changed corpora rebuild.
//! Refreshes single-flight on the refresh lock while concurrent queries keep
//! serving the previous snapshot.

use super::cache::UsageCache;
use super::compact::UsageAssembly;
use super::facts::{FactRow, read_ordinal_run};
use super::merge::{MergedPos, build_merged_order};
use super::progress::{UsageScanProgress, publish_scan_progress};
use super::scan::{
    FileFingerprint, PARSE_SAVE_CHUNK, SCANNERS, deps_observed_current, fingerprint_files,
    parse_missing_usage_files, parse_source_file, reconcile_source_partition,
    run_partition_scanner, source_files, source_ordinal, source_spec, stable_triples,
    usage_file_metadata,
};
use super::{UsageEvent, UsageQuery, usage_timing};
use crate::types::SourceFilter;
use anyhow::Result;
use once_cell::sync::Lazy;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) type PartitionKey = (SourceFilter, Option<PathBuf>);

/// Retained per-source assembly. Immutable once published: queries clone the `Arc`s
/// under a short store lock and then filter and aggregate lock-free, so a cheap
/// query never waits behind a refresh or another expensive report.
pub(crate) struct PartitionEntry {
    /// Last time this entry was served fresh or revalidated. The query TTL decides
    /// when to re-check freshness — not when to discard usable state.
    pub(crate) checked_at: Instant,
    /// Discovery fingerprint (path, size, mtime), sorted by path. Used when no disk
    /// cache backs the query; cache-backed queries revalidate against cache rows.
    pub(crate) fingerprint: FileFingerprint,
    pub(crate) assembly: Arc<UsageAssembly>,
    pub(crate) warnings: Arc<Vec<String>>,
}

/// Bounded per-source snapshot store. Alternating between filters reuses instead of
/// evicting like the previous single slot.
pub(crate) static PARTITIONS: Lazy<Mutex<HashMap<PartitionKey, PartitionEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Maximum retained partitions; the least-recently-checked entry is evicted on
/// insert. Bounds memory across distinct cache paths.
pub(crate) const MAX_PARTITIONS: usize = 32;

/// Precomputed global order over shared partition assemblies, plus the warnings
/// their refreshes reported, in scanner order.
pub(crate) struct MergedSnapshot {
    pub(crate) parts: Vec<Arc<UsageAssembly>>,
    pub(crate) order: Arc<Vec<MergedPos>>,
    pub(crate) warnings: Arc<Vec<String>>,
}

pub(crate) struct MergedEntry {
    checked_at: Instant,
    snapshot: MergedSnapshot,
}

/// Retained combined views, keyed by cache alone: every combined query shares one
/// merged order instead of each rebuilding a full assembly.
pub(crate) static MERGED: Lazy<Mutex<HashMap<Option<PathBuf>, MergedEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Maximum retained merged orders.
pub(crate) const MAX_MERGED: usize = 8;

/// Assembled query input: one partition or the shared merged view.
pub(crate) enum Snapshot {
    Partition(Arc<UsageAssembly>, Arc<Vec<String>>),
    Merged(MergedSnapshot),
}

pub(crate) fn lock_partitions()
-> std::sync::MutexGuard<'static, HashMap<PartitionKey, PartitionEntry>> {
    PARTITIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn lock_merged() -> std::sync::MutexGuard<'static, HashMap<Option<PathBuf>, MergedEntry>>
{
    MERGED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Returns the assembled query input: TTL-fresh snapshots are reused outright,
/// stale ones are revalidated against current file state (reused without any decode
/// when nothing changed), and only genuinely changed corpora pay for a rebuild.
/// Refreshes single-flight on `USAGE_SCAN_LOCK`; concurrent queries keep serving
/// the previous snapshot instead of queueing behind a refresh.
pub(crate) fn ensure_snapshot(query: &UsageQuery) -> Result<Snapshot> {
    let ttl = Duration::from_millis(query.memo_ttl_ms);
    if ttl.is_zero() {
        // One-shot queries bypass retention entirely: no store reads or writes, so
        // they never evict another caller's still-valid snapshot, and no dictionary
        // is built for an assembly nobody will reuse.
        return Ok(oneshot_snapshot(query));
    }
    match query.source {
        Some(filter) => {
            let (assembly, warnings) = ensure_partition(filter, query.cache_path.clone(), ttl);
            Ok(Snapshot::Partition(assembly, warnings))
        }
        None => Ok(Snapshot::Merged(ensure_merged(
            query.cache_path.clone(),
            ttl,
        ))),
    }
}

/// Combined assembly without retention, preserving the previous one-shot path
/// exactly. Single-source one-shots scan only that source.
pub(crate) fn oneshot_snapshot(query: &UsageQuery) -> Snapshot {
    let assembly_start = Instant::now();
    let (events, warnings) = assemble_usage_events(query.source, query.cache_path.as_deref());
    usage_timing(assembly_start, || {
        format!("assemble total ({} events)", events.len())
    });
    match query.source {
        Some(_) => Snapshot::Partition(Arc::new(UsageAssembly::Owned(events)), Arc::new(warnings)),
        // The merged query path works uniformly over partitions; a one-shot
        // combined assembly is just a single-partition merged view.
        None => {
            let assembly = Arc::new(UsageAssembly::Owned(events));
            let order: Vec<MergedPos> = (0..assembly.len())
                .map(|index| MergedPos {
                    part: 0,
                    index: index as u32,
                })
                .collect();
            Snapshot::Merged(MergedSnapshot {
                parts: vec![assembly],
                order: Arc::new(order),
                warnings: Arc::new(warnings),
            })
        }
    }
}

/// Returns one fresh source partition, revalidating or rebuilding as needed.
pub(crate) fn ensure_partition(
    filter: SourceFilter,
    cache_path: Option<PathBuf>,
    ttl: Duration,
) -> (Arc<UsageAssembly>, Arc<Vec<String>>) {
    let key = (filter, cache_path);
    loop {
        let stale = {
            let lock_start = Instant::now();
            let store = lock_partitions();
            usage_timing(lock_start, || "lock wait".to_string());
            match store.get(&key) {
                Some(entry) if entry.checked_at.elapsed() < ttl => {
                    return (entry.assembly.clone(), entry.warnings.clone());
                }
                Some(entry) => Some((entry.assembly.clone(), entry.warnings.clone())),
                None => None,
            }
        };
        match USAGE_SCAN_LOCK.try_lock() {
            Ok(_refresh) => {
                // Another thread may have refreshed between our check and acquiring
                // the refresh lock; re-check before doing any I/O.
                if let Some(entry) = lock_partitions().get(&key)
                    && entry.checked_at.elapsed() < ttl
                {
                    return (entry.assembly.clone(), entry.warnings.clone());
                }
                return refresh_partition(&key);
            }
            Err(_) => {
                // A refresh is already running: serve the previous snapshot instead
                // of queueing behind it. With no snapshot at all, block until the
                // in-flight refresh publishes, then retry.
                if let Some(stale) = stale {
                    return stale;
                }
                let _wait = USAGE_SCAN_LOCK
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }
    }
}

/// Revalidates or rebuilds one partition. Callers must hold `USAGE_SCAN_LOCK`.
pub(crate) fn refresh_partition(key: &PartitionKey) -> (Arc<UsageAssembly>, Arc<Vec<String>>) {
    let (filter, cache_path) = (key.0, key.1.clone());
    let (previous_fingerprint, have_entry) = {
        let store = lock_partitions();
        match store.get(key) {
            Some(entry) => (entry.fingerprint.clone(), true),
            None => (Vec::new(), false),
        }
    };
    let check_start = Instant::now();
    let valid = check_partition_valid(filter, cache_path.as_deref(), &previous_fingerprint);
    // A valid check only reuses when a snapshot exists; otherwise the refresh
    // below still has to build (and publish) one.
    let reuse = valid && have_entry;
    usage_timing(check_start, || {
        format!(
            "{} freshness check ({})",
            filter.as_str(),
            if reuse { "reuse" } else { "refresh" }
        )
    });
    if reuse {
        let mut store = lock_partitions();
        // The entry cannot have been evicted: eviction only happens on publish,
        // which needs the refresh lock we hold.
        if let Some(entry) = store.get_mut(key) {
            entry.checked_at = Instant::now();
            return (entry.assembly.clone(), entry.warnings.clone());
        }
    }
    // Rebuild. The previous snapshot stays published while scanning, so
    // concurrent queries keep serving it; it is taken for buffer reuse only for
    // the compaction window below.
    let mut warnings = Vec::new();
    let mut cache = match cache_path.as_deref().map(UsageCache::open).transpose() {
        Ok(cache) => cache,
        Err(error) => {
            warnings.push(format!("usage cache disabled: {error:#}"));
            None
        }
    };
    // Facts-backed refresh when a sync row exists: hits reuse their fact rows
    // with no blob decode, and only changed files parse. Otherwise the legacy
    // full pipeline, which also backfills facts (e.g. pre-facts databases).
    let stored_warnings = cache.as_ref().and_then(|cache| {
        cache
            .fact_sync(filter.as_str())
            .ok()
            .flatten()
            .map(|(_, _, warnings)| warnings)
    });
    let (events, fingerprint) = match (cache.as_mut(), stored_warnings) {
        (Some(cache), Some(stored)) => {
            match refresh_partition_from_facts(filter, cache, stored, &mut warnings) {
                Ok(done) => done,
                Err(_) => legacy_refresh_partition(filter, Some(cache), &mut warnings),
            }
        }
        (cache, _) => legacy_refresh_partition(filter, cache, &mut warnings),
    };
    // Reuse the previous compact buffers when no concurrent query still holds
    // them; otherwise (the common lock-free case) allocate fresh and let the old
    // snapshot drain naturally.
    let previous = lock_partitions()
        .remove(key)
        .and_then(|entry| Arc::try_unwrap(entry.assembly).ok());
    // Stamp after assembly: an assembly slower than the TTL would otherwise be
    // expired the moment it finishes, and queued follow-up queries would reassemble.
    let compaction_start = Instant::now();
    let event_count = events.len();
    let assembly = Arc::new(UsageAssembly::new(events, previous));
    usage_timing(compaction_start, || {
        format!("{} compact ({event_count} events)", filter.as_str())
    });
    let warnings = Arc::new(warnings);
    let mut store = lock_partitions();
    if store.len() >= MAX_PARTITIONS {
        evict_oldest(&mut store, key, |entry| entry.checked_at);
    }
    store.insert(
        key.clone(),
        PartitionEntry {
            checked_at: Instant::now(),
            fingerprint,
            assembly: assembly.clone(),
            warnings: warnings.clone(),
        },
    );
    (assembly, warnings)
}

/// Legacy full partition rebuild: scan every file (decoding all cached blobs),
/// reconcile, sort, and rewrite facts from scratch. Used when no sync row exists
/// (e.g. pre-facts databases, which this backfills) and as the fallback when a
/// facts-backed refresh hits any error. Returns sorted events plus the discovery
/// fingerprint for the snapshot entry.
pub(crate) fn legacy_refresh_partition(
    filter: SourceFilter,
    mut cache: Option<&mut UsageCache>,
    warnings: &mut Vec<String>,
) -> (Vec<UsageEvent>, FileFingerprint) {
    let scanner = SCANNERS
        .iter()
        .find_map(|(candidate, scanner)| (*candidate == filter).then_some(*scanner))
        .expect("scanner for every source filter");
    let mut events = run_partition_scanner(filter, scanner, warnings, cache.as_deref_mut());
    publish_scan_progress(None);
    let sort_start = Instant::now();
    sort_usage_events(&mut events);
    usage_timing(sort_start, || {
        format!("{} sort ({} events)", filter.as_str(), events.len())
    });
    let fingerprint = discovery_fingerprint(filter);
    // Canonical facts mirror the partition exactly (a failed write only warns,
    // like blob saves; the next refresh rewrites the partition atomically).
    if let Some(cache) = cache {
        let facts_start = Instant::now();
        if let Err(error) = cache.replace_partition_facts(filter, &events, &fingerprint, warnings) {
            warnings.push(format!("{} facts write failed: {error:#}", filter.as_str()));
        }
        usage_timing(facts_start, || {
            format!("{} facts ({} events)", filter.as_str(), events.len())
        });
    }
    (events, fingerprint)
}

/// Facts-backed partition refresh: hits reuse their fact rows with no blob
/// decode, and only changed files parse. Any error returns `Err` so the caller
/// falls back to the legacy path, which reproduces today's exact behavior.
/// Returns sorted events plus the discovery fingerprint for the snapshot entry.
pub(crate) fn refresh_partition_from_facts(
    filter: SourceFilter,
    cache: &mut UsageCache,
    stored_warnings: Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<(Vec<UsageEvent>, FileFingerprint)> {
    let name = filter.as_str();
    let spec = source_spec(filter);
    let files = source_files(filter);
    let parents = (filter == SourceFilter::Codex)
        .then(|| crate::sources::codex::UsageParentIndex::new(&files));
    let now_ms = epoch_ms_now();
    // Stat every file; messages mirror the scan loop exactly.
    let mut examined: Vec<(PathBuf, (u64, i64))> = Vec::with_capacity(files.len());
    for path in &files {
        match usage_file_metadata(path) {
            Ok(metadata) => examined.push((path.clone(), metadata)),
            Err(error) => warnings.push(format!(
                "{name} usage file skipped ({}): {error:#}",
                path.display()
            )),
        }
    }
    let rows = cache
        .load_source_meta(name, spec.parser_version)
        .map_err(|_| anyhow::anyhow!("{name} usage cache read failed"))?;
    let mut dep_observations: HashMap<Vec<u8>, (u64, i64, bool)> = HashMap::new();
    let mut live = HashSet::with_capacity(examined.len());
    let mut missing = Vec::new();
    let mut hit_paths = HashSet::new();
    for (index, (path, metadata)) in examined.iter().enumerate() {
        let key = path.to_string_lossy().to_string();
        let hit = match rows.get(&key) {
            Some(row) => {
                let metadata_current = (spec.volatile_reuse_ms)(path).map_or_else(
                    || (row.size, row.mtime_ns) == *metadata,
                    |window| now_ms.saturating_sub(row.scanned_at_ms) < window,
                );
                metadata_current
                    && deps_observed_current(&row.deps, &mut dep_observations)
                    && (filter != SourceFilter::Codex
                        || parents.as_ref().is_some_and(|parents| {
                            parents.deps_match_current_candidates(&row.deps)
                        }))
            }
            None => false,
        };
        live.insert(key.clone());
        if hit {
            hit_paths.insert(key);
        } else {
            missing.push((index, path.clone(), *metadata));
        }
    }
    // Stale blob rows (vanished files): delete like the scan does.
    let stale: Vec<String> = rows
        .keys()
        .filter(|key| !live.contains(*key))
        .cloned()
        .collect();
    if !stale.is_empty() {
        cache
            .delete_stale(name, &stale)
            .map_err(|_| anyhow::anyhow!("{name} usage cache write failed"))?;
    }
    // Hits' events come from facts (no blob decode); changed and stale rows are
    // excluded by the hit set. Vanished files simply have no rows to read.
    let ordinal = source_ordinal(filter) as i64;
    let mut events: Vec<UsageEvent> = read_ordinal_run(&cache.connection, ordinal, None, None)
        .map_err(|_| anyhow::anyhow!("{name} facts read failed"))?
        .into_iter()
        .filter(|row| hit_paths.contains(&row.path))
        .map(FactRow::into_event)
        .collect();
    // Parse what changed, with chunked saves mirroring the scan loop.
    let parse = |path: &Path| parse_source_file(filter, path, parents.as_ref());
    let mut parsed_paths: Vec<String> = Vec::new();
    let mut parsed_events: Vec<UsageEvent> = Vec::new();
    if !missing.is_empty() {
        publish_scan_progress(Some(UsageScanProgress {
            source: name,
            done: 0,
            total: missing.len(),
        }));
    }
    let mut save_warned = false;
    let parse_start = Instant::now();
    let missing_count = missing.len();
    for chunk in missing.chunks(PARSE_SAVE_CHUNK) {
        let parsed = parse_missing_usage_files(name, chunk, warnings, &parse);
        if parsed.iter().any(|file| file.cacheable)
            && let Err(error) = cache.save_batch(name, spec.parser_version, now_ms, &parsed)
            && !save_warned
        {
            save_warned = true;
            warnings.push(format!("{name} usage cache write failed: {error:#}"));
        }
        for file in parsed {
            parsed_paths.push(file.path.to_string_lossy().to_string());
            parsed_events.extend(file.events);
        }
    }
    if missing_count > 0 {
        usage_timing(parse_start, || {
            format!("{name} parse ({missing_count} changed files)")
        });
    }
    publish_scan_progress(None);
    events.extend(parsed_events);
    let reconcile_start = Instant::now();
    reconcile_source_partition(filter, &mut events);
    usage_timing(reconcile_start, || {
        format!("{} reconcile ({} events)", filter.as_str(), events.len())
    });
    let sort_start = Instant::now();
    sort_usage_events(&mut events);
    usage_timing(sort_start, || {
        format!("{} sort ({} events)", filter.as_str(), events.len())
    });
    // Rewrite facts for changed, vanished, and skipped (unstatable) files; hits
    // keep their rows. The fingerprint covers exactly the statted files,
    // matching what a future check recomputes.
    let mut fingerprint: FileFingerprint = examined
        .iter()
        .map(|(path, (size, mtime_ns))| (path.to_string_lossy().to_string(), *size, *mtime_ns))
        .collect();
    fingerprint.sort();
    let mut removed: HashSet<String> = parsed_paths.into_iter().collect();
    removed.extend(stale);
    for path in &files {
        let key = path.to_string_lossy().to_string();
        if !live.contains(&key) {
            // Discovered but unstatable: warned above, dropped like stale rows.
            removed.insert(key);
        }
    }
    let mut changed: Vec<UsageEvent> = Vec::new();
    for event in &events {
        let path: &str = event.source_path.as_ref();
        if removed.contains(path) {
            changed.push(event.clone());
        }
    }
    // Stored warnings describe unchanged files; new warnings describe this
    // refresh's parses. Union them deduplicated (see write_fact_sync).
    let mut stored = stored_warnings;
    for warning in warnings.iter() {
        if !stored.contains(warning) {
            stored.push(warning.clone());
        }
    }
    let upsert_start = Instant::now();
    cache
        .upsert_file_facts(
            filter,
            &removed.into_iter().collect::<Vec<_>>(),
            &changed,
            &fingerprint,
            &stored,
        )
        .map_err(|_| anyhow::anyhow!("{name} facts write failed"))?;
    usage_timing(upsert_start, || {
        format!(
            "{} facts upsert ({} events)",
            filter.as_str(),
            changed.len()
        )
    });
    Ok((events, fingerprint))
}

/// Returns the fresh merged view over all source partitions, rebuilding the order
/// only when some partition changed. Combined queries reference the same per-source
/// snapshots instead of requiring a separate full assembly.
pub(crate) fn ensure_merged(cache_path: Option<PathBuf>, ttl: Duration) -> MergedSnapshot {
    loop {
        let stale = {
            let store = lock_merged();
            match store.get(&cache_path) {
                Some(entry) if entry.checked_at.elapsed() < ttl => {
                    return clone_merged(&entry.snapshot);
                }
                Some(entry) => Some(clone_merged(&entry.snapshot)),
                None => None,
            }
        };
        match USAGE_SCAN_LOCK.try_lock() {
            Ok(_refresh) => {
                if let Some(entry) = lock_merged().get(&cache_path)
                    && entry.checked_at.elapsed() < ttl
                {
                    return clone_merged(&entry.snapshot);
                }
                return refresh_merged(&cache_path);
            }
            Err(_) => {
                if let Some(stale) = stale {
                    return stale;
                }
                let _wait = USAGE_SCAN_LOCK
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }
    }
}

pub(crate) fn clone_merged(snapshot: &MergedSnapshot) -> MergedSnapshot {
    MergedSnapshot {
        parts: snapshot.parts.clone(),
        order: snapshot.order.clone(),
        warnings: snapshot.warnings.clone(),
    }
}

/// Brings every partition up to date, then reuses or rebuilds the merged order.
/// Callers must hold `USAGE_SCAN_LOCK`; partition refreshes reuse the same lock
/// instead of their own try-lock, so this never deadlocks.
pub(crate) fn refresh_merged(cache_path: &Option<PathBuf>) -> MergedSnapshot {
    let mut parts = Vec::with_capacity(SCANNERS.len());
    let mut warnings = Vec::new();
    for (filter, _) in SCANNERS {
        let (assembly, partition_warnings) = refresh_partition(&(filter, cache_path.clone()));
        warnings.extend(partition_warnings.iter().cloned());
        parts.push(assembly);
    }
    let current: Vec<Arc<UsageAssembly>> = parts;
    let mut store = lock_merged();
    if let Some(entry) = store.get_mut(cache_path)
        && entry.snapshot.parts.len() == current.len()
        && entry
            .snapshot
            .parts
            .iter()
            .zip(current.iter())
            .all(|(old, new)| Arc::ptr_eq(old, new))
    {
        entry.checked_at = Instant::now();
        return clone_merged(&entry.snapshot);
    }
    let order = Arc::new(build_merged_order(&current));
    let snapshot = MergedSnapshot {
        parts: current,
        order,
        warnings: Arc::new(warnings),
    };
    if store.len() >= MAX_MERGED {
        evict_oldest(&mut store, cache_path, |entry| entry.checked_at);
    }
    store.insert(
        cache_path.clone(),
        MergedEntry {
            checked_at: Instant::now(),
            snapshot: clone_merged(&snapshot),
        },
    );
    snapshot
}

/// Evicts the least-recently-checked entry, never the key being published.
pub(crate) fn evict_oldest<K, V>(
    store: &mut HashMap<K, V>,
    keep: &K,
    checked_at: impl Fn(&V) -> Instant,
) where
    K: Clone + Eq + std::hash::Hash,
{
    let oldest = store
        .iter()
        .filter(|(key, _)| *key != keep)
        .min_by_key(|(_, entry)| checked_at(entry))
        .map(|(key, _)| key.clone());
    if let Some(key) = oldest {
        store.remove(&key);
    }
}

/// Partition freshness without decoding payloads. With a disk cache, the facts
/// sync fingerprint decides for plain log files (a new/removed/extended file, or
/// a parser bump, changes it), while volatile databases additionally consult
/// their rows' reuse windows exactly like the scan does. New parent copies and
/// vanished files change the discovered set, so fork-set completeness and stale
/// rows fall out of the fingerprint with no dependency bookkeeping here. Without
/// a disk cache, compares the discovery fingerprint against the stored one. Any
/// I/O hiccup (or a missing sync row, e.g. a pre-facts database) returns false
/// so the refresh reproduces today's exact behavior instead of reusing on
/// uncertain ground.
pub(crate) fn check_partition_valid(
    filter: SourceFilter,
    cache_path: Option<&Path>,
    previous_fingerprint: &[(String, u64, i64)],
) -> bool {
    let files = source_files(filter);
    let mut fingerprint = Vec::with_capacity(files.len());
    for path in &files {
        let Ok(metadata) = usage_file_metadata(path) else {
            return false;
        };
        fingerprint.push((path.to_string_lossy().to_string(), metadata.0, metadata.1));
    }
    fingerprint.sort();
    let Some(cache_path) = cache_path else {
        // Without a disk cache there are no rows to compare: the discovery
        // fingerprint alone decides.
        return fingerprint == previous_fingerprint;
    };
    let spec = source_spec(filter);
    let cache = match UsageCache::open(cache_path) {
        Ok(cache) => cache,
        Err(_) => return false,
    };
    let expected = fingerprint_files(&stable_triples(filter, &fingerprint));
    match cache.fact_sync(filter.as_str()) {
        Ok(Some((recorded, version, _))) => {
            if recorded != expected || version != spec.parser_version {
                return false;
            }
        }
        _ => return false,
    }
    // Volatile databases are judged by reuse windows, not fingerprints: their
    // bytes can change under an unchanged mtime while WAL content is pending.
    if files
        .iter()
        .any(|path| (spec.volatile_reuse_ms)(path).is_some())
    {
        let rows = match cache.load_source_meta(filter.as_str(), spec.parser_version) {
            Ok(rows) => rows,
            Err(_) => return false,
        };
        let now_ms = epoch_ms_now();
        for path in &files {
            let Some(window) = (spec.volatile_reuse_ms)(path) else {
                continue;
            };
            let key = path.to_string_lossy().to_string();
            match rows.get(&key) {
                Some(row) if now_ms.saturating_sub(row.scanned_at_ms) < window => {}
                _ => return false,
            }
        }
    }
    true
}

/// Current discovery fingerprint for one source, sorted by path.
pub(crate) fn discovery_fingerprint(filter: SourceFilter) -> FileFingerprint {
    let mut fingerprint = Vec::new();
    for path in source_files(filter) {
        if let Ok(metadata) = usage_file_metadata(&path) {
            fingerprint.push((path.to_string_lossy().to_string(), metadata.0, metadata.1));
        }
    }
    fingerprint.sort();
    fingerprint
}

pub(crate) fn assemble_usage_events(
    source: Option<SourceFilter>,
    cache_path: Option<&Path>,
) -> (Vec<UsageEvent>, Vec<String>) {
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    let mut cache = match cache_path.map(UsageCache::open).transpose() {
        Ok(cache) => cache,
        Err(error) => {
            warnings.push(format!("usage cache disabled: {error:#}"));
            None
        }
    };
    for (filter, scanner) in SCANNERS {
        if source.is_none_or(|selected| selected == filter) {
            events.extend(run_partition_scanner(
                filter,
                scanner,
                &mut warnings,
                cache.as_mut(),
            ));
        }
    }
    publish_scan_progress(None);
    let sort_start = Instant::now();
    sort_usage_events(&mut events);
    usage_timing(sort_start, || "sort".to_string());
    (events, warnings)
}

/// Preserve stable event ordering without the full event-sized scratch allocation
/// used by a parallel merge sort. Sorting indices also avoids repeatedly moving the
/// large owned records. Original positions break equal-key ties exactly as before.
pub(crate) fn sort_usage_events(events: &mut [UsageEvent]) {
    let mut order: Vec<usize> = (0..events.len()).collect();
    order.par_sort_unstable_by(|&left, &right| {
        let a = &events[left];
        let b = &events[right];
        (a.timestamp_ms, &a.source_path, a.source_order)
            .cmp(&(b.timestamp_ms, &b.source_path, b.source_order))
            .then_with(|| left.cmp(&right))
    });
    // Each entry maps a destination to its original position. Follow each cycle,
    // placing its next record and marking visited positions as fixed points.
    for start in 0..order.len() {
        let mut current = start;
        loop {
            let next = order[current];
            order[current] = current;
            if next == start {
                break;
            }
            events.swap(current, next);
            current = next;
        }
    }
}

/// Refresh/publication lock. Held only while revalidating or rebuilding a snapshot —
/// never across filtering, aggregation, or visitor callbacks. Concurrent queries
/// clone snapshot `Arc`s under the short store lock and run lock-free.
pub(crate) static USAGE_SCAN_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub(crate) fn epoch_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
