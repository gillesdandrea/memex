//! Reconstructed local token usage.
//!
//! This module intentionally does not model provider quota percentages. Local logs are useful for
//! request-level accounting, but they are not authoritative subscription-limit telemetry.

use crate::analytics::ProjectGrouping;
use crate::types::SourceFilter;
use anyhow::Result;
use clap::ValueEnum;
use once_cell::sync::Lazy;
use rayon::prelude::*;
use rusqlite::{Connection, params, params_from_iter};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod compact;
use compact::{FilterFields, UsageAssembly, UsageEventView};

#[derive(Clone, Debug, Default)]
pub struct UsageQuery {
    pub source: Option<SourceFilter>,
    pub project: Option<String>,
    pub project_grouping: ProjectGrouping,
    pub session_keys: Option<HashSet<(String, String)>>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub cost_mode: CostMode,
    pub include_events: bool,
    /// Include internal AI permission-review sessions in reconstructed usage.
    pub include_reviews: bool,
    pub cache_path: Option<PathBuf>,
    /// How long a retained snapshot may be served before its freshness is re-checked.
    /// A re-check that finds nothing changed reuses the snapshot without any decode,
    /// so this is a check interval, not a discard deadline. Filters (`since_ms`,
    /// `project`, `session_keys`, ...) apply after assembly, so repeated queries over
    /// the same corpus share one scan. Zero bypasses retention entirely (one-shot).
    pub memo_ttl_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum CostMode {
    Source,
    #[default]
    Auto,
    Reprice,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TokenBuckets {
    /// Provider-reported input. For OpenAI-shaped records this includes the cached subset.
    pub raw_input: u64,
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// One-hour cache writes, a subset of `cache_write`.
    pub cache_write_1h: u64,
    /// Billable output, including reasoning when a provider reports it separately.
    pub output: u64,
    /// Reasoning output, retained as a subset of `output` for reporting.
    pub reasoning: u64,
}

impl TokenBuckets {
    pub(crate) fn additive_total(&self) -> u64 {
        self.uncached_input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }

    pub fn total(&self) -> u64 {
        self.additive_total()
    }

    pub(crate) fn codex(input: u64, cached: u64, output: u64, reasoning: u64) -> Self {
        let cache_read = cached.min(input);
        Self {
            raw_input: input,
            uncached_input: input.saturating_sub(cache_read),
            cache_read,
            cache_write: 0,
            cache_write_1h: 0,
            output,
            reasoning,
        }
    }

    pub(crate) fn disjoint(input: u64, cache_read: u64, cache_write: u64, output: u64) -> Self {
        Self {
            raw_input: input,
            uncached_input: input,
            cache_read,
            cache_write,
            cache_write_1h: 0,
            output,
            reasoning: 0,
        }
    }
}

pub type UsageEvent = UsageEventData<String, Arc<str>>;

/// The same event fields are used by parsers, borrowed report views, and compact storage.
/// Only the text representation changes; the public event and wire formats stay owned.
#[derive(Clone, Debug, Serialize)]
pub struct UsageEventData<S, P> {
    pub source: &'static str,
    /// Shared across every event of a file: assembled scans materialize millions of
    /// events, and per-event owned paths dominated allocation time.
    pub source_path: P,
    pub source_record_id: Option<S>,
    pub session_id: Option<S>,
    pub request_id: Option<S>,
    pub message_id: Option<S>,
    pub timestamp_ms: u64,
    pub project: Option<S>,
    pub provider: Option<S>,
    pub model: Option<S>,
    pub tokens: TokenBuckets,
    pub source_cost_usd: Option<f64>,
    /// A missing source cost is intentionally covered by an authoritative aggregate.
    #[serde(skip)]
    pub(crate) cost_authoritative: bool,
    pub dedupe_confidence: &'static str,
    pub conservative_undercount: bool,
    /// The source row is an aggregate rather than one request in a cache chain.
    #[serde(skip)]
    pub(crate) cache_chain_excluded: bool,
    #[serde(skip)]
    pub(crate) sidechain: bool,
    #[serde(skip)]
    pub(crate) permission_review: bool,
    #[serde(skip)]
    pub(crate) source_order: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageSummary {
    pub source: String,
    pub events: u64,
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub total_tokens: u64,
    pub known_cost_usd: f64,
    pub priced_events: u64,
    pub unpriced_events: u64,
    pub cache_waste: CacheWaste,
}

/// Estimated prompt-cache waste: prompt tokens that were in the previous request's prompt
/// (so a warm cache would have served them as cache reads) but were re-billed at
/// input/cache-write rates instead.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CacheWaste {
    pub missed_tokens: u64,
    /// Extra USD paid vs. a full cache hit, at catalog rates; misses on unpriced models
    /// contribute tokens but no cost.
    pub missed_cost_usd: f64,
    /// Misses above the per-request noise floor.
    pub miss_count: u64,
    /// Misses following an idle gap of at least the cache TTL (same model).
    pub idle_misses: u64,
    /// Misses where the model changed relative to the previous request.
    pub model_switch_misses: u64,
}

impl CacheWaste {
    fn absorb(&mut self, other: &CacheWaste) {
        self.missed_tokens = self.missed_tokens.saturating_add(other.missed_tokens);
        self.missed_cost_usd += other.missed_cost_usd;
        self.miss_count += other.miss_count;
        self.idle_misses += other.idle_misses;
        self.model_switch_misses += other.model_switch_misses;
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageReport {
    pub authority: &'static str,
    pub events: u64,
    pub total_tokens: u64,
    pub unknown_model_events: u64,
    pub conservative_events: u64,
    pub cost_mode: CostMode,
    pub price_catalog: &'static str,
    pub known_cost_usd: f64,
    pub priced_events: u64,
    pub unpriced_events: u64,
    pub cache_waste: CacheWaste,
    pub by_source: Vec<UsageSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<UsageEvent>,
    pub warnings: Vec<String>,
}

/// One filtered usage event projected to what activity charts need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageActivityPoint {
    pub source: &'static str,
    pub timestamp_ms: u64,
    pub total_tokens: u64,
}

/// Filters the assembled events exactly like `scan_usage`, but returns lightweight chart
/// points instead of deep-cloning full events out of the memoized assembly. The boolean is
/// true when any scanner reported a warning, i.e. the totals may be partial.
pub fn scan_usage_activity(query: &UsageQuery) -> Result<(Vec<UsageActivityPoint>, bool)> {
    let (points, warnings) = scan_usage_activity_with_warnings(query)?;
    Ok((points, !warnings.is_empty()))
}

pub(crate) fn scan_usage_activity_with_warnings(
    query: &UsageQuery,
) -> Result<(Vec<UsageActivityPoint>, Vec<String>)> {
    let mut points = Vec::new();
    let warnings = visit_usage_activity(query, |point| points.push(point))?;
    Ok((points, warnings))
}

/// Visit matching chart points without allocating a per-event result vector.
/// The callback runs outside any usage lock over a shared snapshot, so it may safely
/// start another usage query (each acquisition is short); it must still avoid
/// mutating state the scan itself reads.
pub fn visit_usage_activity(
    query: &UsageQuery,
    mut visit: impl FnMut(UsageActivityPoint),
) -> Result<Vec<String>> {
    // Cold-start fast path before touching snapshots (verification mode below
    // always computes both paths instead).
    if !facts_read_enabled()
        && let Some((points, warnings)) = try_cold_serve_points(query)?
    {
        for point in points {
            visit(point);
        }
        return Ok(warnings);
    }
    // Order-4A verification: answer from facts when enabled, self-checked
    // against the assembly points with fallback.
    if facts_read_enabled()
        && let Some(cache_path) = query.cache_path.as_deref()
    {
        let mut assembly_points = Vec::new();
        let warnings = visit_inner(query, &mut |point| assembly_points.push(point))?;
        match read_fact_points(query, cache_path) {
            Ok(facts_points) if facts_points == assembly_points => {
                for point in facts_points {
                    visit(point);
                }
            }
            _ => {
                for point in assembly_points {
                    visit(point);
                }
            }
        }
        return Ok(warnings);
    }
    visit_inner(query, visit)
}

/// Cold-start fast path for activity points. Warms snapshots for retaining
/// queries; one-shots serve without retaining.
fn try_cold_serve_points(
    query: &UsageQuery,
) -> Result<Option<(Vec<UsageActivityPoint>, Vec<String>)>> {
    let Some(ready) = cold_facts_ready(query) else {
        return Ok(None);
    };
    let cache_path = query
        .cache_path
        .as_deref()
        .expect("cold serve needs a cache");
    let points = read_fact_points(query, cache_path)?;
    if query.memo_ttl_ms != 0 {
        populate_snapshots_from_facts(query.source, cache_path, &ready.per_source);
    }
    Ok(Some((points, ready.warnings)))
}

fn visit_inner(
    query: &UsageQuery,
    mut visit: impl FnMut(UsageActivityPoint),
) -> Result<Vec<String>> {
    match ensure_snapshot(query)? {
        Snapshot::Partition(assembled, warnings) => {
            for index in filtered_events(&assembled, query) {
                visit(assembled.activity_point(index));
            }
            Ok(warnings.as_ref().clone())
        }
        Snapshot::Merged(merged) => {
            let view = MergedView {
                parts: &merged.parts,
                order: &merged.order,
            };
            for pos in filtered_merged_positions(view, query) {
                visit(merged.parts[pos.part as usize].activity_point(pos.index as usize));
            }
            Ok(merged.warnings.as_ref().clone())
        }
    }
}

/// Chart projection: sort key plus only what activity points need. Project and
/// session predicates need the full row, so filtered charts use full runs.
struct FactPoint {
    source: &'static str,
    path: String,
    source_order: u64,
    timestamp_ms: u64,
    permission_review: bool,
    total_tokens: u64,
}

fn read_fact_point_runs(
    cache_path: &Path,
    source: Option<SourceFilter>,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
) -> Result<Vec<Vec<FactPoint>>> {
    let connection = Connection::open(cache_path)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let mut runs = Vec::new();
    for (filter, _) in SCANNERS {
        if source.is_none_or(|selected| selected == filter) {
            let mut query = String::from(
                "SELECT ordinal, timestamp_ms, path, source_order, permission_review,
                        uncached_input, cache_read, cache_write, output
                 FROM usage_facts WHERE ordinal = ?",
            );
            let mut params: Vec<rusqlite::types::Value> =
                vec![(source_ordinal(filter) as i64).into()];
            if let Some(since) = since_ms {
                query.push_str(" AND timestamp_ms >= ?");
                params.push((since as i64).into());
            }
            if let Some(until) = until_ms {
                query.push_str(" AND timestamp_ms < ?");
                params.push((until as i64).into());
            }
            query.push_str(" ORDER BY ordinal, timestamp_ms, path, source_order");
            let mut statement = connection.prepare(&query)?;
            let rows = statement
                .query_map(params_from_iter(params), |row| {
                    let ordinal: i64 = row.get(0)?;
                    // Same buckets as TokenBuckets::additive_total (reasoning is
                    // an output subset, not billed twice).
                    let total = (row.get::<_, i64>(5)? as u64)
                        .saturating_add(row.get::<_, i64>(6)? as u64)
                        .saturating_add(row.get::<_, i64>(7)? as u64)
                        .saturating_add(row.get::<_, i64>(8)? as u64);
                    Ok((
                        ordinal,
                        FactPoint {
                            source: "",
                            path: row.get(2)?,
                            source_order: row.get::<_, i64>(3)? as u64,
                            timestamp_ms: row.get::<_, i64>(1)? as u64,
                            permission_review: row.get::<_, i64>(4)? != 0,
                            total_tokens: total,
                        },
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut run = Vec::with_capacity(rows.len());
            for (ordinal, mut point) in rows {
                let Some(label) = fact_source_label(ordinal) else {
                    continue;
                };
                point.source = label;
                run.push(point);
            }
            runs.push(run);
        }
    }
    Ok(runs)
}

/// Stored per-source warnings in scanner order, for cold-start reports. `None`
/// on any gap (missing sync row, unreadable database): the caller falls back to
/// a normal scan, which heals it.
fn stored_warnings_for_scope(
    source: Option<SourceFilter>,
    cache_path: &Path,
) -> Option<Vec<(SourceFilter, Vec<String>)>> {
    let cache = UsageCache::open(cache_path).ok()?;
    let mut per_source = Vec::new();
    for (filter, _) in SCANNERS {
        if source.is_none_or(|selected| selected == filter) {
            let (_, _, warnings) = cache.fact_sync(filter.as_str()).ok()??;
            per_source.push((filter, warnings));
        }
    }
    Some(per_source)
}

/// Cold-start prerequisites: no usable in-memory snapshot, but facts current on
/// disk for the whole scope, plus the stored warnings to answer with.
struct ColdFacts {
    warnings: Vec<String>,
    per_source: Vec<(SourceFilter, Vec<String>)>,
}

/// Cold-start fast path prerequisites: no usable in-memory snapshot, but facts
/// current on disk for the whole scope.
/// Validity here mirrors what a rebuild would find (same fingerprint scheme);
/// any doubt returns `None` and the normal path rebuilds instead.
fn cold_facts_ready(query: &UsageQuery) -> Option<ColdFacts> {
    let cache_path = query.cache_path.as_deref()?;
    let ttl = Duration::from_millis(query.memo_ttl_ms);
    if !ttl.is_zero() {
        // Usable snapshots beat facts reads (borrowed views, no mapping), so
        // cold-serve only when the normal path would have to build.
        match query.source {
            Some(filter) => {
                if lock_partitions().contains_key(&(filter, query.cache_path.clone())) {
                    return None;
                }
            }
            None => {
                if lock_merged().contains_key(&query.cache_path) {
                    return None;
                }
                let store = lock_partitions();
                if SCANNERS
                    .iter()
                    .all(|(filter, _)| store.contains_key(&(*filter, query.cache_path.clone())))
                {
                    // All partitions resident: the normal path only re-checks and
                    // merges, which is cheaper than a facts read.
                    return None;
                }
            }
        }
    }
    // Whole-scope validity, then stored warnings. Either failing falls back.
    for (filter, _) in SCANNERS {
        if query.source.is_none_or(|selected| selected == filter)
            && !check_partition_valid(filter, Some(cache_path), &[])
        {
            return None;
        }
    }
    let per_source = stored_warnings_for_scope(query.source, cache_path)?;
    let warnings = per_source
        .iter()
        .flat_map(|(_, warnings)| warnings.iter().cloned())
        .collect();
    Some(ColdFacts {
        warnings,
        per_source,
    })
}

/// Build snapshot assemblies from facts for amortization: map rows to owned
/// events (already canonical and ordered, so no reconcile or sort) and compact.
/// Best-effort and non-blocking — if a refresh is already running, it will
/// publish anyway. Failures simply leave the store empty for the next attempt.
fn populate_snapshots_from_facts(
    source: Option<SourceFilter>,
    cache_path: &Path,
    per_source: &[(SourceFilter, Vec<String>)],
) {
    let Ok(_refresh) = USAGE_SCAN_LOCK.try_lock() else {
        return;
    };
    for (filter, warnings) in per_source {
        if lock_partitions().contains_key(&(*filter, Some(cache_path.to_path_buf()))) {
            continue;
        }
        let Ok(runs) = read_fact_runs(cache_path, Some(*filter), None, None) else {
            continue;
        };
        let events: Vec<UsageEvent> = runs
            .into_iter()
            .flatten()
            .map(FactRow::into_event)
            .collect();
        let fingerprint = discovery_fingerprint(*filter);
        let assembly = Arc::new(UsageAssembly::new(events, None));
        let mut store = lock_partitions();
        if store.len() >= MAX_PARTITIONS {
            let key = (*filter, Some(cache_path.to_path_buf()));
            evict_oldest(&mut store, &key, |entry| entry.checked_at);
        }
        store.insert(
            (*filter, Some(cache_path.to_path_buf())),
            PartitionEntry {
                checked_at: Instant::now(),
                fingerprint,
                assembly,
                warnings: Arc::new(warnings.clone()),
            },
        );
    }
    if source.is_none() {
        // Index over the fresh assemblies; partition checks inside reuse them.
        refresh_merged(&Some(cache_path.to_path_buf()));
    }
}

fn read_fact_points(query: &UsageQuery, cache_path: &Path) -> Result<Vec<UsageActivityPoint>> {
    // Unfiltered charts (no project/session predicates) use the narrow
    // projection: same merged order, far less mapping per row.
    if query.project.is_none() && query.session_keys.is_none() {
        let runs = read_fact_point_runs(cache_path, query.source, query.since_ms, query.until_ms)?;
        let order = merge_runs(
            &runs,
            |point| (point.timestamp_ms, point.path.as_str(), point.source_order),
            "fact points merge",
        );
        let mut plan = FilterPlan::new(query);
        return Ok(order
            .iter()
            .map(|pos| &runs[pos.part as usize][pos.index as usize])
            .filter(|point| {
                plan.matches(FilterFields {
                    source: point.source,
                    permission_review: point.permission_review,
                    project: None,
                    session_id: None,
                })
            })
            .map(|point| UsageActivityPoint {
                source: point.source,
                timestamp_ms: point.timestamp_ms,
                total_tokens: point.total_tokens,
            })
            .collect());
    }
    let runs = read_fact_runs(cache_path, query.source, query.since_ms, query.until_ms)?;
    let order = merge_fact_runs(&runs);
    let mut plan = FilterPlan::new(query);
    Ok(order
        .iter()
        .map(|pos| &runs[pos.part as usize][pos.index as usize])
        .filter(|row| plan.matches(row.filter_fields()))
        .map(|row| UsageActivityPoint {
            source: row.source,
            timestamp_ms: row.timestamp_ms,
            total_tokens: row.tokens.total(),
        })
        .collect())
}

/// Query predicates with per-query precomputations shared by single-assembly and
/// merged filtering: the normalized project query, borrowed session keys, and the
/// repository-project cache.
struct FilterPlan<'a> {
    grouping: ProjectGrouping,
    include_reviews: bool,
    project_raw: Option<&'a str>,
    project_key: Option<String>,
    session_set: Option<HashSet<(&'a str, &'a str)>>,
    project_cache: HashMap<String, String>,
}

impl<'a> FilterPlan<'a> {
    fn new(query: &'a UsageQuery) -> Self {
        // Normalize the project query once instead of per event.
        let project_raw = query.project.as_deref();
        let project_key = project_raw.map(usage_project_key);
        // Borrow session keys once; lookups below avoid per-event String allocation.
        let session_set: Option<HashSet<(&str, &str)>> = query.session_keys.as_ref().map(|keys| {
            keys.iter()
                .map(|(source, session)| (source.as_str(), session.as_str()))
                .collect()
        });
        Self {
            grouping: query.project_grouping,
            include_reviews: query.include_reviews,
            project_raw,
            project_key,
            session_set,
            project_cache: HashMap::new(),
        }
    }

    fn matches(&mut self, event: FilterFields<'_>) -> bool {
        (self.include_reviews || !event.permission_review)
            && self.project_raw.is_none_or(|project| {
                self.project_key.as_deref().is_some_and(|key| {
                    event.project.is_some_and(|candidate| {
                        usage_project_matches_precomputed(
                            candidate,
                            project,
                            key,
                            self.grouping,
                            &mut self.project_cache,
                        )
                    })
                })
            })
            && self.session_set.as_ref().is_none_or(|keys| {
                event
                    .session_id
                    .is_some_and(|session_id| keys.contains(&(event.source, session_id)))
            })
    }
}

/// Assembled events are already sorted; filtering preserves that order.
/// Timestamp bounds use binary search so narrow memo queries avoid walking all history.
fn filtered_events<'a>(
    assembled: &'a UsageAssembly,
    query: &'a UsageQuery,
) -> impl Iterator<Item = usize> + 'a {
    let start = query
        .since_ms
        .map_or(0, |since| assembled.lower_bound(since));
    let end = query
        .until_ms
        .map_or(assembled.len(), |until| assembled.upper_bound(until, start));
    let start = start.min(assembled.len());
    let end = end.clamp(start, assembled.len());
    let mut plan = FilterPlan::new(query);
    (start..end).filter(move |&index| plan.matches(assembled.filter_fields(index)))
}

/// Position in a merged multi-partition order: which partition and which index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MergedPos {
    part: u32,
    index: u32,
}

/// K-way merge of timestamp-sorted partitions into global order. Ties across
/// partitions break by scanner ordinal then partition index, reproducing the
/// combined assembly's stable sort exactly (partitions are laid out in scanner
/// order there, with `(timestamp, path, order)` unique per event).
fn build_merged_order(parts: &[Arc<UsageAssembly>]) -> Vec<MergedPos> {
    let merge_start = Instant::now();
    // Partitions must arrive sorted (snapshot refreshes sort them); the merge only
    // reproduces the combined stable sort on that precondition.
    debug_assert!(parts.iter().all(|assembly| {
        (1..assembly.len()).all(|index| assembly.sort_key(index - 1) <= assembly.sort_key(index))
    }));
    let total: usize = parts.iter().map(|assembly| assembly.len()).sum();
    let mut order = Vec::with_capacity(total);
    // Current head per non-empty partition; thirteen partitions make a linear
    // minimum scan competitive with a heap, and trivially correct.
    let mut heads: Vec<(usize, usize)> = parts
        .iter()
        .enumerate()
        .filter(|(_, assembly)| assembly.len() > 0)
        .map(|(part, _)| (part, 0))
        .collect();
    while !heads.is_empty() {
        let mut best = 0;
        for candidate in 1..heads.len() {
            let (part, index) = heads[candidate];
            let (best_part, best_index) = heads[best];
            let key = parts[part].sort_key(index);
            let best_key = parts[best_part].sort_key(best_index);
            if (key, part, index) < (best_key, best_part, best_index) {
                best = candidate;
            }
        }
        let (part, index) = heads[best];
        order.push(MergedPos {
            part: part as u32,
            index: index as u32,
        });
        if index + 1 < parts[part].len() {
            heads[best] = (part, index + 1);
        } else {
            heads.swap_remove(best);
        }
    }
    usage_timing(merge_start, || format!("merge ({} events)", order.len()));
    order
}

/// Borrowed view over merged partitions plus their precomputed global order.
struct MergedView<'a> {
    parts: &'a [Arc<UsageAssembly>],
    order: &'a [MergedPos],
}

impl MergedView<'_> {
    fn len(&self) -> usize {
        self.order.len()
    }

    fn lower_bound(&self, since: u64) -> usize {
        self.order.partition_point(|pos| {
            self.parts[pos.part as usize].timestamp_ms(pos.index as usize) < since
        })
    }

    fn upper_bound(&self, until: u64, start: usize) -> usize {
        start
            + self.order[start..].partition_point(|pos| {
                self.parts[pos.part as usize].timestamp_ms(pos.index as usize) < until
            })
    }
}

/// Merged positions matching a query, in global order. Timestamp bounds use binary
/// search over the merged order so narrow queries avoid walking all history.
fn filtered_merged_positions<'a>(
    view: MergedView<'a>,
    query: &'a UsageQuery,
) -> impl Iterator<Item = MergedPos> + 'a {
    let start = query.since_ms.map_or(0, |since| view.lower_bound(since));
    let end = query
        .until_ms
        .map_or(view.len(), |until| view.upper_bound(until, start));
    let start = start.min(view.len());
    let end = end.clamp(start, view.len());
    let mut plan = FilterPlan::new(query);
    let parts = view.parts;
    view.order[start..end]
        .iter()
        .copied()
        .filter(move |pos| plan.matches(parts[pos.part as usize].filter_fields(pos.index as usize)))
}

pub fn scan_usage(query: &UsageQuery) -> Result<UsageReport> {
    // Cold-start fast path: valid facts on disk but nothing usable in memory —
    // answer without decoding, sorting, or compacting anything. Verification
    // mode always computes both paths instead, so it stays below that gate.
    if !facts_read_enabled()
        && let Some(report) = try_cold_serve_report(query)?
    {
        return Ok(report);
    }
    let snapshot_start = Instant::now();
    let snapshot = ensure_snapshot(query)?;
    usage_timing(snapshot_start, || "snapshot ensure".to_string());
    let report = match snapshot {
        Snapshot::Partition(assembled, warnings) => {
            scan_single(query, &assembled, warnings.as_ref())
        }
        Snapshot::Merged(merged) => scan_merged(query, merged),
    }?;
    // Order-4A verification: when enabled, answer from canonical facts and
    // self-check the totals against the assembly report. Any incompleteness
    // (e.g. facts predating this binary) falls back to the assembly report.
    if facts_read_enabled()
        && let Some(cache_path) = query.cache_path.as_deref()
    {
        let facts_start = Instant::now();
        let facts_report = scan_usage_from_facts(query, cache_path, &report.warnings);
        usage_timing(facts_start, || "facts report".to_string());
        match facts_report {
            Ok(facts_report)
                if facts_report.events == report.events
                    && facts_report.total_tokens == report.total_tokens =>
            {
                return Ok(facts_report);
            }
            // Incomplete facts (e.g. rows predating this binary): the assembly
            // report stands. Order 4B makes facts primary with backfill.
            _ => {
                usage_timing(facts_start, || "facts fallback".to_string());
            }
        }
    }
    Ok(report)
}

/// Whether reports read canonical facts instead of assembled events. Order-4A
/// verification path: populate-then-read must digest-match the assembly path.
fn facts_read_enabled() -> bool {
    static ENABLED: Lazy<bool> =
        Lazy::new(|| std::env::var_os("MEMEX_USAGE_FACTS_READ").is_some_and(|value| value != "0"));
    *ENABLED
}

/// Cold-start fast path for reports: when no usable snapshot exists but facts
/// are current on disk, answer from facts without decoding, sorting, or
/// compacting anything. Warms the snapshot store for follow-up queries when the
/// query retains (non-zero TTL); one-shot queries serve without retaining.
fn try_cold_serve_report(query: &UsageQuery) -> Result<Option<UsageReport>> {
    let Some(ready) = cold_facts_ready(query) else {
        return Ok(None);
    };
    let cache_path = query
        .cache_path
        .as_deref()
        .expect("cold serve needs a cache");
    let report = scan_usage_from_facts(query, cache_path, &ready.warnings)?;
    if query.memo_ttl_ms != 0 {
        populate_snapshots_from_facts(query.source, cache_path, &ready.per_source);
    }
    Ok(Some(report))
}

fn scan_single(
    query: &UsageQuery,
    assembled: &UsageAssembly,
    warnings: &[String],
) -> Result<UsageReport> {
    let filter_start = Instant::now();
    let events: Vec<usize> = filtered_events(assembled, query).collect();
    usage_timing(filter_start, || {
        format!("filter ({} of {} events)", events.len(), assembled.len())
    });

    let mut by_source: HashMap<&'static str, UsageSummary> = HashMap::new();
    let mut report = UsageReport {
        authority: "local_log",
        cost_mode: query.cost_mode,
        price_catalog: PRICE_CATALOG_ID,
        warnings: warnings.to_vec(),
        ..UsageReport::default()
    };
    let mut rate_cache = RateCache::default();
    let pricing_start = Instant::now();
    for event in events.iter().map(|&index| assembled.view(index)) {
        accumulate_usage_event(
            &mut report,
            &mut by_source,
            &event,
            query.cost_mode,
            &mut rate_cache,
        );
    }
    usage_timing(pricing_start, || {
        format!("pricing ({} events)", events.len())
    });
    let waste_start = Instant::now();
    for (source, waste) in compute_cache_waste(events.iter().map(|&index| assembled.view(index))) {
        report.cache_waste.absorb(&waste);
        if let Some(row) = by_source.get_mut(&source) {
            row.cache_waste = waste;
        }
    }
    usage_timing(waste_start, || "cache waste".to_string());
    report.by_source = by_source.into_values().collect();
    report.by_source.sort_by(|a, b| a.source.cmp(&b.source));
    if query.include_events {
        report.details = assembled.details(events.into_iter());
    }
    Ok(report)
}

/// Shared per-event totals aggregation so single and merged reports cannot drift.
fn accumulate_usage_event(
    report: &mut UsageReport,
    by_source: &mut HashMap<&'static str, UsageSummary>,
    event: &UsageEventView<'_>,
    cost_mode: CostMode,
    rate_cache: &mut RateCache,
) {
    let total = event.tokens.additive_total();
    report.events += 1;
    report.total_tokens = report.total_tokens.saturating_add(total);
    report.unknown_model_events += u64::from(event.model.is_none());
    report.conservative_events += u64::from(event.conservative_undercount);
    let cost = event_cost_nanos_cached(event, cost_mode, rate_cache);
    if let Some(cost) = cost {
        report.priced_events += 1;
        report.known_cost_usd += cost as f64 / 1_000_000_000.0;
    } else {
        report.unpriced_events += 1;
    }
    let row = by_source
        .entry(event.source)
        .or_insert_with(|| UsageSummary {
            source: event.source.to_string(),
            ..UsageSummary::default()
        });
    row.events += 1;
    row.uncached_input = row
        .uncached_input
        .saturating_add(event.tokens.uncached_input);
    row.cache_read = row.cache_read.saturating_add(event.tokens.cache_read);
    row.cache_write = row.cache_write.saturating_add(event.tokens.cache_write);
    row.output = row.output.saturating_add(event.tokens.output);
    row.reasoning = row.reasoning.saturating_add(event.tokens.reasoning);
    row.total_tokens = row.total_tokens.saturating_add(total);
    if let Some(cost) = cost {
        row.priced_events += 1;
        row.known_cost_usd += cost as f64 / 1_000_000_000.0;
    } else {
        row.unpriced_events += 1;
    }
}

fn scan_merged(query: &UsageQuery, merged: MergedSnapshot) -> Result<UsageReport> {
    let MergedSnapshot {
        parts,
        order,
        warnings,
    } = merged;
    let view = MergedView {
        parts: &parts,
        order: &order,
    };
    let total = view.len();
    let filter_start = Instant::now();
    let matches: Vec<MergedPos> = filtered_merged_positions(view, query).collect();
    usage_timing(filter_start, || {
        format!("filter ({} of {} events)", matches.len(), total)
    });

    let mut by_source: HashMap<&'static str, UsageSummary> = HashMap::new();
    let mut report = UsageReport {
        authority: "local_log",
        cost_mode: query.cost_mode,
        price_catalog: PRICE_CATALOG_ID,
        warnings: warnings.as_ref().clone(),
        ..UsageReport::default()
    };
    let view_at = |pos: &MergedPos| parts[pos.part as usize].view(pos.index as usize);
    let mut rate_cache = RateCache::default();
    let pricing_start = Instant::now();
    for event in matches.iter().map(view_at) {
        accumulate_usage_event(
            &mut report,
            &mut by_source,
            &event,
            query.cost_mode,
            &mut rate_cache,
        );
    }
    usage_timing(pricing_start, || {
        format!("pricing ({} events)", matches.len())
    });
    let waste_start = Instant::now();
    for (source, waste) in compute_cache_waste(matches.iter().map(view_at)) {
        report.cache_waste.absorb(&waste);
        if let Some(row) = by_source.get_mut(&source) {
            row.cache_waste = waste;
        }
    }
    usage_timing(waste_start, || "cache waste".to_string());
    report.by_source = by_source.into_values().collect();
    report.by_source.sort_by(|a, b| a.source.cmp(&b.source));
    if query.include_events {
        report.details = merged_details(&parts, &matches);
    }
    Ok(report)
}

/// One canonical usage fact row: every field reports need, in owned form.
struct FactRow {
    source: &'static str,
    path: String,
    source_order: u64,
    timestamp_ms: u64,
    session_id: Option<String>,
    project: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    source_record_id: Option<String>,
    request_id: Option<String>,
    message_id: Option<String>,
    tokens: TokenBuckets,
    source_cost_usd: Option<f64>,
    cost_authoritative: bool,
    dedupe_confidence: &'static str,
    conservative_undercount: bool,
    cache_chain_excluded: bool,
    sidechain: bool,
    permission_review: bool,
}

fn fact_source_label(ordinal: i64) -> Option<&'static str> {
    usize::try_from(ordinal)
        .ok()
        .and_then(|index| SCANNERS.get(index))
        .map(|(filter, _)| filter.as_str())
}

const FACT_COLUMNS: &str =
    "ordinal, path, source_order, timestamp_ms, session_id, project, provider,
        model, source_record_id, request_id, message_id, raw_input, uncached_input,
        cache_read, cache_write, cache_write_1h, output, reasoning, source_cost_usd,
        cost_authoritative, dedupe_confidence, conservative_undercount,
        cache_chain_excluded, sidechain, permission_review";

fn map_fact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, FactRow)> {
    let ordinal: i64 = row.get(0)?;
    let as_u64 = |index: usize| row.get::<_, i64>(index).map(|value| value as u64);
    let dedupe: String = row.get(20)?;
    Ok((
        ordinal,
        FactRow {
            // Resolved by the caller; unknown ordinals are quarantined.
            source: "",
            path: row.get(1)?,
            source_order: as_u64(2)?,
            timestamp_ms: as_u64(3)?,
            session_id: row.get(4)?,
            project: row.get(5)?,
            provider: row.get(6)?,
            model: row.get(7)?,
            source_record_id: row.get(8)?,
            request_id: row.get(9)?,
            message_id: row.get(10)?,
            tokens: TokenBuckets {
                raw_input: as_u64(11)?,
                uncached_input: as_u64(12)?,
                cache_read: as_u64(13)?,
                cache_write: as_u64(14)?,
                cache_write_1h: as_u64(15)?,
                output: as_u64(16)?,
                reasoning: as_u64(17)?,
            },
            source_cost_usd: row.get(18)?,
            cost_authoritative: row.get::<_, i64>(19)? != 0,
            dedupe_confidence: match dedupe.as_str() {
                "exact" => "exact",
                "strong" => "strong",
                _ => "heuristic",
            },
            conservative_undercount: row.get::<_, i64>(21)? != 0,
            cache_chain_excluded: row.get::<_, i64>(22)? != 0,
            sidechain: row.get::<_, i64>(23)? != 0,
            permission_review: row.get::<_, i64>(24)? != 0,
        },
    ))
}

/// One source's facts in report order, streamed straight from the composite
/// index: no SQLite sort step at any size. Runs are positional; for combined
/// queries every source is read in scanner order so positions are ordinals.
fn read_fact_runs(
    cache_path: &Path,
    source: Option<SourceFilter>,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
) -> Result<Vec<Vec<FactRow>>> {
    let read_start = Instant::now();
    let connection = Connection::open(cache_path)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let mut runs = Vec::new();
    for (filter, _) in SCANNERS {
        if source.is_none_or(|selected| selected == filter) {
            runs.push(read_ordinal_run(
                &connection,
                source_ordinal(filter) as i64,
                since_ms,
                until_ms,
            )?);
        }
    }
    usage_timing(read_start, || {
        format!(
            "facts read ({} rows)",
            runs.iter().map(Vec::len).sum::<usize>()
        )
    });
    Ok(runs)
}

/// One source's facts in report order from an open connection. Shared by report
/// reads and refreshes rebuilding a partition without decoding blobs.
fn read_ordinal_run(
    connection: &Connection,
    ordinal: i64,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
) -> Result<Vec<FactRow>> {
    let mut query = format!("SELECT {FACT_COLUMNS} FROM usage_facts WHERE ordinal = ?");
    let mut params: Vec<rusqlite::types::Value> = vec![ordinal.into()];
    if let Some(since) = since_ms {
        query.push_str(" AND timestamp_ms >= ?");
        params.push((since as i64).into());
    }
    if let Some(until) = until_ms {
        query.push_str(" AND timestamp_ms < ?");
        params.push((until as i64).into());
    }
    query.push_str(" ORDER BY ordinal, timestamp_ms, path, source_order");
    let mut statement = connection.prepare(&query)?;
    let rows = statement
        .query_map(params_from_iter(params), map_fact_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut run = Vec::with_capacity(rows.len());
    for (row_ordinal, mut fact) in rows {
        // Unknown ordinals (foreign or corrupt rows) are quarantined, like
        // corrupt blobs: skipping them can only undercount on damage.
        let Some(label) = fact_source_label(row_ordinal) else {
            continue;
        };
        fact.source = label;
        run.push(fact);
    }
    Ok(run)
}

/// K-way merge of per-source ordered runs into global order. Same key and
/// tiebreaks as the assembly merge (runs arrive in scanner order, so positional
/// tiebreaks are scanner ordinals), so float sums and chains match exactly.
fn merge_runs<T>(
    runs: &[Vec<T>],
    key: impl for<'row> Fn(&'row T) -> (u64, &'row str, u64),
    label: &'static str,
) -> Vec<MergedPos> {
    let merge_start = Instant::now();
    let total: usize = runs.iter().map(Vec::len).sum();
    let mut order = Vec::with_capacity(total);
    let mut heads: Vec<(usize, usize)> = runs
        .iter()
        .enumerate()
        .filter(|(_, run)| !run.is_empty())
        .map(|(part, _)| (part, 0))
        .collect();
    while !heads.is_empty() {
        let mut best = 0;
        for candidate in 1..heads.len() {
            let (part, index) = heads[candidate];
            let (best_part, best_index) = heads[best];
            let row_key = key(&runs[part][index]);
            let best_key = key(&runs[best_part][best_index]);
            if (row_key, part, index) < (best_key, best_part, best_index) {
                best = candidate;
            }
        }
        let (part, index) = heads[best];
        order.push(MergedPos {
            part: part as u32,
            index: index as u32,
        });
        if index + 1 < runs[part].len() {
            heads[best] = (part, index + 1);
        } else {
            heads.swap_remove(best);
        }
    }
    usage_timing(merge_start, || format!("{label} ({} rows)", order.len()));
    order
}

/// K-way merge of per-source ordered full fact runs into global order.
fn merge_fact_runs(runs: &[Vec<FactRow>]) -> Vec<MergedPos> {
    merge_runs(
        runs,
        |row| (row.timestamp_ms, row.path.as_str(), row.source_order),
        "facts merge",
    )
}

impl FactRow {
    fn filter_fields(&self) -> FilterFields<'_> {
        FilterFields {
            source: self.source,
            permission_review: self.permission_review,
            project: self.project.as_deref(),
            session_id: self.session_id.as_deref(),
        }
    }

    fn view(&self) -> UsageEventView<'_> {
        UsageEventData {
            source: self.source,
            source_path: self.path.as_str(),
            source_record_id: self.source_record_id.as_deref(),
            session_id: self.session_id.as_deref(),
            request_id: self.request_id.as_deref(),
            message_id: self.message_id.as_deref(),
            timestamp_ms: self.timestamp_ms,
            project: self.project.as_deref(),
            provider: self.provider.as_deref(),
            model: self.model.as_deref(),
            tokens: self.tokens.clone(),
            source_cost_usd: self.source_cost_usd,
            cost_authoritative: self.cost_authoritative,
            dedupe_confidence: self.dedupe_confidence,
            conservative_undercount: self.conservative_undercount,
            cache_chain_excluded: self.cache_chain_excluded,
            sidechain: self.sidechain,
            permission_review: self.permission_review,
            source_order: self.source_order,
        }
    }

    fn into_event(self) -> UsageEvent {
        UsageEvent {
            source: self.source,
            source_path: Arc::from(self.path.as_str()),
            source_record_id: self.source_record_id,
            session_id: self.session_id,
            request_id: self.request_id,
            message_id: self.message_id,
            timestamp_ms: self.timestamp_ms,
            project: self.project,
            provider: self.provider,
            model: self.model,
            tokens: self.tokens,
            source_cost_usd: self.source_cost_usd,
            cost_authoritative: self.cost_authoritative,
            dedupe_confidence: self.dedupe_confidence,
            conservative_undercount: self.conservative_undercount,
            cache_chain_excluded: self.cache_chain_excluded,
            sidechain: self.sidechain,
            permission_review: self.permission_review,
            source_order: self.source_order,
        }
    }
}

fn scan_usage_from_facts(
    query: &UsageQuery,
    cache_path: &Path,
    warnings: &[String],
) -> Result<UsageReport> {
    let runs = read_fact_runs(cache_path, query.source, query.since_ms, query.until_ms)?;
    let order = merge_fact_runs(&runs);
    let row_at = |pos: &MergedPos| &runs[pos.part as usize][pos.index as usize];
    let mut plan = FilterPlan::new(query);
    let mut report = UsageReport {
        authority: "local_log",
        cost_mode: query.cost_mode,
        price_catalog: PRICE_CATALOG_ID,
        warnings: warnings.to_vec(),
        ..UsageReport::default()
    };
    let mut by_source: HashMap<&'static str, UsageSummary> = HashMap::new();
    let mut rate_cache = RateCache::default();
    // Time bounds already applied in SQL; remaining predicates match row-for-row
    // with the assembly path, in the same order.
    let matched: Vec<MergedPos> = order
        .iter()
        .copied()
        .filter(|pos| plan.matches(row_at(pos).filter_fields()))
        .collect();
    for event in matched.iter().map(|pos| row_at(pos).view()) {
        accumulate_usage_event(
            &mut report,
            &mut by_source,
            &event,
            query.cost_mode,
            &mut rate_cache,
        );
    }
    for (source, waste) in compute_cache_waste(matched.iter().map(|pos| row_at(pos).view())) {
        report.cache_waste.absorb(&waste);
        if let Some(row) = by_source.get_mut(&source) {
            row.cache_waste = waste;
        }
    }
    report.by_source = by_source.into_values().collect();
    report.by_source.sort_by(|a, b| a.source.cmp(&b.source));
    if query.include_events {
        report.details = fact_details(runs, &matched);
    }
    Ok(report)
}

/// Detail rows for merged fact matches in global order. Matches are grouped per
/// run (each sublist stays ascending) and converted in batches, then interleaved
/// back into merged order with moves instead of clones.
fn fact_details(runs: Vec<Vec<FactRow>>, matches: &[MergedPos]) -> Vec<UsageEvent> {
    use std::collections::VecDeque;
    let mut per_run: Vec<Vec<usize>> = vec![Vec::new(); runs.len()];
    for pos in matches {
        per_run[pos.part as usize].push(pos.index as usize);
    }
    let mut batched: Vec<VecDeque<UsageEvent>> = runs
        .into_iter()
        .zip(per_run)
        .map(|(run, indices)| {
            run.into_iter()
                .enumerate()
                .filter(|(index, _)| indices.binary_search(index).is_ok())
                .map(|(_, row)| row.into_event())
                .collect()
        })
        .collect();
    let mut details = Vec::with_capacity(matches.len());
    for pos in matches {
        details.push(
            batched[pos.part as usize]
                .pop_front()
                .expect("batched details cover every match"),
        );
    }
    details
}

/// Detail rows for merged matches in global order. Matches are grouped per
/// partition (each sublist stays ascending) for batched details — preserving the
/// per-file path sharing — then interleaved back into merged order with moves.
fn merged_details(parts: &[Arc<UsageAssembly>], matches: &[MergedPos]) -> Vec<UsageEvent> {
    use std::collections::VecDeque;
    let mut per_part: Vec<Vec<usize>> = vec![Vec::new(); parts.len()];
    for pos in matches {
        per_part[pos.part as usize].push(pos.index as usize);
    }
    let mut batched: Vec<VecDeque<UsageEvent>> = parts
        .iter()
        .zip(per_part)
        .map(|(assembly, indices)| assembly.details(indices.into_iter()).into())
        .collect();
    let mut details = Vec::with_capacity(matches.len());
    for pos in matches {
        details.push(
            batched[pos.part as usize]
                .pop_front()
                .expect("batched details cover every match"),
        );
    }
    details
}

/// Prompt-cache TTL: misses after idle gaps at least this long are attributed to expiry.
/// Anthropic's default cache TTL is 5 minutes.
const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Per-request misses at or below this are cache breakpoint granularity noise.
const CACHE_MISS_NOISE_FLOOR_TOKENS: u64 = 1024;

/// The last request seen in a session chain; everything in its prompt should be cached.
struct CacheChainState<'a> {
    prompt_tokens: u64,
    /// (provider, model); a change re-bills the full prompt and is counted as a miss.
    model: (&'a str, &'a str),
    timestamp_ms: u64,
    /// Sticky: some earlier request in this chain reported cache activity. Distinguishes a
    /// total miss on a read-only-reporting provider (OpenAI-style, writes unreported) from
    /// a provider that never reports caching at all.
    reported_cache: bool,
}

/// Estimate per-source cache waste by chaining each session's requests in order and
/// comparing every request's cache reads against the previous request's prompt.
///
/// This follows pi's cache-stats algorithm (earendil-works/pi, core/cache-stats.ts) with
/// adaptations for reconstructed logs: sidechain requests are excluded (subagents have
/// their own prompt caches), conservatively undercounted events break the chain (their
/// buckets are clamped dedupe deltas, not a real request's shape), and a prompt shrinking
/// below half of its predecessor stands in for the compaction/clear markers the logs don't
/// carry — the context legitimately changed, so the re-billing is not counted as waste.
/// Chains start at the first event a caller passes in, so window filters only undercount at
/// their leading edge.
fn compute_cache_waste<'a>(
    events: impl IntoIterator<Item = UsageEventView<'a>>,
) -> HashMap<&'static str, CacheWaste> {
    let mut chains: HashMap<(&'a str, &'a str, &'a str), CacheChainState<'a>> = HashMap::new();
    let mut by_source: HashMap<&'static str, CacheWaste> = HashMap::new();
    let mut rate_cache = RateCache::default();
    for event in events {
        if event.sidechain {
            continue;
        }
        let Some(session_id) = event.session_id else {
            continue;
        };
        // A chain is one process's linear request stream, which is the transcript file, not
        // the session: codex spawned/resumed threads share a session id across rollout
        // files, and interleaving them fabricates misses. OpenCode is the exception — it
        // persists one file per message, so there the session is the stream.
        let thread = if event.source == "opencode" {
            ""
        } else {
            event.source_path
        };
        let key = (event.source, session_id, thread);
        if event.cache_chain_excluded {
            chains.remove(&key);
            continue;
        }
        if event.conservative_undercount {
            chains.remove(&key);
            continue;
        }
        let tokens = &event.tokens;
        let prompt_tokens = tokens
            .uncached_input
            .saturating_add(tokens.cache_read)
            .saturating_add(tokens.cache_write);
        if prompt_tokens == 0 {
            continue;
        }
        let cached = tokens.cache_read.saturating_add(tokens.cache_write);
        let model = (event.provider.unwrap_or(""), event.model.unwrap_or(""));
        let mut reported_cache = cached > 0;
        if let Some(prev) = chains.get(&key) {
            reported_cache |= prev.reported_cache;
            // A current cache write alone doesn't qualify: the chain's first write creates
            // the cache, so the previous prompt could not have been served from it. A read
            // proves a cache already existed (OpenAI-style writes are unreported), as does
            // earlier reported activity.
            if (tokens.cache_read > 0 || prev.reported_cache)
                && prompt_tokens.saturating_mul(2) >= prev.prompt_tokens
            {
                let missed = prev
                    .prompt_tokens
                    .min(prompt_tokens)
                    .saturating_sub(tokens.cache_read);
                if missed > CACHE_MISS_NOISE_FLOOR_TOKENS {
                    let waste = by_source.entry(event.source).or_default();
                    waste.miss_count += 1;
                    waste.missed_tokens = waste.missed_tokens.saturating_add(missed);
                    waste.missed_cost_usd +=
                        cache_miss_cost_usd_cached(&event, missed, &mut rate_cache);
                    if model != prev.model {
                        waste.model_switch_misses += 1;
                    } else if event.timestamp_ms.saturating_sub(prev.timestamp_ms) >= CACHE_TTL_MS {
                        waste.idle_misses += 1;
                    }
                }
            }
        }
        chains.insert(
            key,
            CacheChainState {
                prompt_tokens,
                model,
                timestamp_ms: event.timestamp_ms,
                reported_cache,
            },
        );
    }
    by_source
}

/// Extra USD paid for `missed_tokens` vs. reading them from cache. Missed tokens can only
/// land in the uncached-input or cache-write buckets, so the paid rate is the blend of this
/// event's own paid buckets at catalog rates; 0 when the model is unpriced.
#[allow(dead_code)]
fn cache_miss_cost_usd(event: &UsageEventView<'_>, missed_tokens: u64) -> f64 {
    let mut cache = RateCache::default();
    cache_miss_cost_usd_cached(event, missed_tokens, &mut cache)
}

fn cache_miss_cost_usd_cached(
    event: &UsageEventView<'_>,
    missed_tokens: u64,
    cache: &mut RateCache,
) -> f64 {
    let Some(model) = event.model else {
        return 0.0;
    };
    let Some(rates) = cache.rates_for(event.provider, model) else {
        return 0.0;
    };
    let cache_write_1h = event.tokens.cache_write_1h.min(event.tokens.cache_write);
    let cache_write_5m = event.tokens.cache_write - cache_write_1h;
    let paid_tokens = event
        .tokens
        .uncached_input
        .saturating_add(event.tokens.cache_write);
    if paid_tokens == 0 {
        return 0.0;
    }
    // Rates are nano-USD per million tokens; dividing by a million yields nano-USD per token.
    let paid_nanos = ((event.tokens.uncached_input as u128) * (rates.input as u128)
        + (cache_write_5m as u128) * (rates.cache_write_5m as u128)
        + (cache_write_1h as u128) * (rates.cache_write_1h as u128)) as f64
        / 1_000_000.0;
    let paid_per_token = paid_nanos / paid_tokens as f64;
    let read_per_token = rates.cache_read as f64 / 1_000_000.0;
    missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0) / 1_000_000_000.0
}
type PartitionKey = (SourceFilter, Option<PathBuf>);

/// Retained per-source assembly. Immutable once published: queries clone the `Arc`s
/// under a short store lock and then filter and aggregate lock-free, so a cheap
/// query never waits behind a refresh or another expensive report.
struct PartitionEntry {
    /// Last time this entry was served fresh or revalidated. The query TTL decides
    /// when to re-check freshness — not when to discard usable state.
    checked_at: Instant,
    /// Discovery fingerprint (path, size, mtime), sorted by path. Used when no disk
    /// cache backs the query; cache-backed queries revalidate against cache rows.
    fingerprint: FileFingerprint,
    assembly: Arc<UsageAssembly>,
    warnings: Arc<Vec<String>>,
}

/// Bounded per-source snapshot store. Alternating between filters reuses instead of
/// evicting like the previous single slot.
static PARTITIONS: Lazy<Mutex<HashMap<PartitionKey, PartitionEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Maximum retained partitions; the least-recently-checked entry is evicted on
/// insert. Bounds memory across distinct cache paths.
const MAX_PARTITIONS: usize = 32;

/// Precomputed global order over shared partition assemblies, plus the warnings
/// their refreshes reported, in scanner order.
struct MergedSnapshot {
    parts: Vec<Arc<UsageAssembly>>,
    order: Arc<Vec<MergedPos>>,
    warnings: Arc<Vec<String>>,
}

struct MergedEntry {
    checked_at: Instant,
    snapshot: MergedSnapshot,
}

/// Retained combined views, keyed by cache alone: every combined query shares one
/// merged order instead of each rebuilding a full assembly.
static MERGED: Lazy<Mutex<HashMap<Option<PathBuf>, MergedEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Maximum retained merged orders.
const MAX_MERGED: usize = 8;

/// Assembled query input: one partition or the shared merged view.
enum Snapshot {
    Partition(Arc<UsageAssembly>, Arc<Vec<String>>),
    Merged(MergedSnapshot),
}

fn lock_partitions() -> std::sync::MutexGuard<'static, HashMap<PartitionKey, PartitionEntry>> {
    PARTITIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_merged() -> std::sync::MutexGuard<'static, HashMap<Option<PathBuf>, MergedEntry>> {
    MERGED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Returns the assembled query input: TTL-fresh snapshots are reused outright,
/// stale ones are revalidated against current file state (reused without any decode
/// when nothing changed), and only genuinely changed corpora pay for a rebuild.
/// Refreshes single-flight on `USAGE_SCAN_LOCK`; concurrent queries keep serving
/// the previous snapshot instead of queueing behind a refresh.
fn ensure_snapshot(query: &UsageQuery) -> Result<Snapshot> {
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
fn oneshot_snapshot(query: &UsageQuery) -> Snapshot {
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
fn ensure_partition(
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
fn refresh_partition(key: &PartitionKey) -> (Arc<UsageAssembly>, Arc<Vec<String>>) {
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
fn legacy_refresh_partition(
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
fn refresh_partition_from_facts(
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
fn ensure_merged(cache_path: Option<PathBuf>, ttl: Duration) -> MergedSnapshot {
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

fn clone_merged(snapshot: &MergedSnapshot) -> MergedSnapshot {
    MergedSnapshot {
        parts: snapshot.parts.clone(),
        order: snapshot.order.clone(),
        warnings: snapshot.warnings.clone(),
    }
}

/// Brings every partition up to date, then reuses or rebuilds the merged order.
/// Callers must hold `USAGE_SCAN_LOCK`; partition refreshes reuse the same lock
/// instead of their own try-lock, so this never deadlocks.
fn refresh_merged(cache_path: &Option<PathBuf>) -> MergedSnapshot {
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
fn evict_oldest<K, V>(store: &mut HashMap<K, V>, keep: &K, checked_at: impl Fn(&V) -> Instant)
where
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
fn check_partition_valid(
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
fn discovery_fingerprint(filter: SourceFilter) -> FileFingerprint {
    let mut fingerprint = Vec::new();
    for path in source_files(filter) {
        if let Ok(metadata) = usage_file_metadata(&path) {
            fingerprint.push((path.to_string_lossy().to_string(), metadata.0, metadata.1));
        }
    }
    fingerprint.sort();
    fingerprint
}

fn assemble_usage_events(
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

type SourceScanner =
    fn(&mut Vec<UsageEvent>, &mut Vec<String>, Option<&mut UsageCache>) -> Result<()>;

/// Scanner ordinals double as merge tiebreaks: partitions are laid out and merged in
/// this order, reproducing the combined assembly's stable sort exactly.
const SCANNERS: [(SourceFilter, SourceScanner); 13] = [
    (SourceFilter::Claude, scan_claude),
    (SourceFilter::Codex, scan_codex),
    (SourceFilter::Opencode, scan_opencode),
    (SourceFilter::Pi, scan_pi),
    (SourceFilter::Omp, scan_omp),
    (SourceFilter::OpenClaw, scan_openclaw),
    (SourceFilter::Cursor, scan_cursor),
    (SourceFilter::Copilot, scan_copilot),
    (SourceFilter::Grok, scan_grok),
    (SourceFilter::Hermes, scan_hermes),
    (SourceFilter::Jcode, scan_jcode),
    (SourceFilter::Muse, scan_muse),
    (SourceFilter::Antigravity, scan_antigravity),
];

/// Scan and reconcile one source partition. Shared by combined assembly and
/// per-source snapshot refreshes so both observe identical per-source pipelines.
/// The partition is left unsorted: combined assembly sorts globally, while snapshot
/// refreshes sort the partition (see `refresh_partition`).
fn run_partition_scanner(
    filter: SourceFilter,
    scanner: SourceScanner,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Vec<UsageEvent> {
    let scanner_start = Instant::now();
    // Reconcile each source's partition before combining so Claude's keep
    // bitmap and multi-pass scans cover only its own events, not all history.
    let mut partition = Vec::new();
    let result = scanner(&mut partition, warnings, cache);
    if let Err(error) = result {
        warnings.push(format!("{} scanner: {error:#}", filter.as_str()));
        partition.clear();
    } else {
        let reconcile_start = Instant::now();
        reconcile_source_partition(filter, &mut partition);
        usage_timing(reconcile_start, || {
            format!("{} reconcile ({} events)", filter.as_str(), partition.len())
        });
    }
    usage_timing(scanner_start, || format!("{} scanner", filter.as_str()));
    partition
}

fn reconcile_source_partition(filter: SourceFilter, events: &mut Vec<UsageEvent>) {
    match filter {
        SourceFilter::Claude => crate::sources::claude::reconcile_usage(events),
        SourceFilter::Codex => crate::sources::codex::reconcile_usage(events),
        SourceFilter::Cursor => crate::sources::cursor::reconcile_usage(events),
        SourceFilter::Copilot => crate::sources::copilot::reconcile_usage(events),
        SourceFilter::Opencode => crate::sources::opencode::reconcile_usage(events),
        _ => {}
    }
}

/// Preserve stable event ordering without the full event-sized scratch allocation
/// used by a parallel merge sort. Sorting indices also avoids repeatedly moving the
/// large owned records. Original positions break equal-key ties exactly as before.
fn sort_usage_events(events: &mut [UsageEvent]) {
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

/// When `MEMEX_USAGE_TIMING` is set (and not "0"), prints per-phase scan timings to
/// stderr. In the TUI, redirect stderr to a file (`MEMEX_USAGE_TIMING=1 memex 2>/tmp/t.log`)
/// so the lines don't corrupt the terminal.
fn usage_timing(start: Instant, message: impl FnOnce() -> String) {
    static ENABLED: Lazy<bool> =
        Lazy::new(|| std::env::var_os("MEMEX_USAGE_TIMING").is_some_and(|value| value != "0"));
    if *ENABLED {
        eprintln!(
            "usage-timing {} {}ms",
            message(),
            start.elapsed().as_millis()
        );
    }
}

#[allow(dead_code)]
fn usage_project_matches(
    candidate: &str,
    project: &str,
    grouping: ProjectGrouping,
    cache: &mut HashMap<String, String>,
) -> bool {
    let project_key = usage_project_key(project);
    usage_project_matches_precomputed(candidate, project, &project_key, grouping, cache)
}

fn usage_project_matches_precomputed(
    candidate: &str,
    project_raw: &str,
    project_key: &str,
    grouping: ProjectGrouping,
    cache: &mut HashMap<String, String>,
) -> bool {
    if candidate.eq_ignore_ascii_case(project_raw) {
        return true;
    }
    match grouping {
        ProjectGrouping::Flat => project_tail(candidate).eq_ignore_ascii_case(project_key),
        ProjectGrouping::Repository => {
            if let Some(cached) = cache.get(candidate) {
                return cached.eq_ignore_ascii_case(project_key);
            }
            let computed = if Path::new(candidate).is_absolute() {
                crate::analytics::repository_project_for_cwd(candidate)
                    .unwrap_or_else(|| crate::analytics::UNFILED_PROJECT.to_string())
            } else {
                project_tail(candidate).to_string()
            };
            let matched = computed.eq_ignore_ascii_case(project_key);
            cache.insert(candidate.to_string(), computed);
            matched
        }
    }
}

fn starts_with_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn project_tail(value: &str) -> &str {
    let trimmed = value.trim().trim_end_matches(['/', '\\']);
    let mut tail = trimmed.rsplit(['/', '\\', ':']).next().unwrap_or(trimmed);
    tail = tail.strip_suffix(".git").unwrap_or(tail);
    let encoded = tail.trim_matches('-');
    if tail.starts_with('-')
        && (starts_with_ignore_ascii_case(encoded, "users-")
            || starts_with_ignore_ascii_case(encoded, "home-"))
    {
        return encoded.rsplit('-').next().unwrap_or(encoded);
    }
    tail
}

fn usage_project_key(value: &str) -> String {
    project_tail(value).to_string()
}

/// Reuse cached Cursor state databases this long even when their metadata changed: a
/// running Cursor rewrites its (potentially multi-GB) databases continuously, and
/// re-reading them on every scan makes live scans unusable.
/// Expiry forces a read even when main-file metadata is unchanged: committed
/// SQLite writes can remain entirely in the WAL until checkpoint.
const VOLATILE_DB_REUSE_MS: i64 = 60_000;
/// Cache rows are persisted after every chunk of parsed files, not once per source, so an
/// interrupted cold scan resumes from the last completed chunk instead of starting over.
const PARSE_SAVE_CHUNK: usize = 128;
/// Refresh/publication lock. Held only while revalidating or rebuilding a snapshot —
/// never across filtering, aggregation, or visitor callbacks. Concurrent queries
/// clone snapshot `Arc`s under the short store lock and run lock-free.
static USAGE_SCAN_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// Parse-phase progress of the usage scan currently holding `USAGE_SCAN_LOCK`. Cache hits
/// are not counted: progress is only published while files are being (re)parsed, which is
/// the phase that can take minutes on a cold cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct UsageScanProgress {
    pub source: &'static str,
    pub done: usize,
    pub total: usize,
}

static USAGE_SCAN_PROGRESS: Lazy<Mutex<Option<UsageScanProgress>>> = Lazy::new(|| Mutex::new(None));

pub fn usage_scan_progress() -> Option<UsageScanProgress> {
    *USAGE_SCAN_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Forward advancing cold-cache counters without imposing a total scan lifetime.
/// Callers retain their own cancellation and inactivity watchdogs.
pub(crate) fn with_usage_progress<T: Send>(
    action: impl FnOnce() -> T + Send,
    mut publish: impl FnMut(UsageScanProgress) -> Result<()>,
) -> Result<T> {
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        scope.spawn(move || {
            let _ = sender.send(action());
        });
        let mut previous = None;
        loop {
            match receiver.recv_timeout(Duration::from_millis(200)) {
                Ok(result) => return Ok(result),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow::anyhow!("activity worker stopped without a result"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(progress) = usage_scan_progress()
                        && Some(progress) != previous
                    {
                        publish(progress)?;
                        previous = Some(progress);
                    }
                }
            }
        }
    })
}

fn publish_scan_progress(progress: Option<UsageScanProgress>) {
    *USAGE_SCAN_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = progress;
}

pub(crate) fn publish_remote_scan_progress(progress: UsageScanProgress) {
    publish_scan_progress(Some(progress));
}

fn bump_scan_progress() {
    if let Some(progress) = USAGE_SCAN_PROGRESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        progress.done += 1;
    }
}

#[derive(Serialize, Deserialize)]
struct CachedUsageEvent {
    source_record_id: Option<String>,
    session_id: Option<String>,
    request_id: Option<String>,
    message_id: Option<String>,
    timestamp_ms: u64,
    project: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    tokens: TokenBuckets,
    source_cost_usd: Option<f64>,
    cost_authoritative: bool,
    dedupe_confidence: String,
    conservative_undercount: bool,
    cache_chain_excluded: bool,
    sidechain: bool,
    permission_review: bool,
    source_order: u64,
}

/// Borrowed serialization view with the same Postcard layout as `CachedUsageEvent`.
/// `save_batch` serializes through this type to avoid cloning every string field.
#[derive(Serialize)]
struct CachedUsageEventRef<'a> {
    source_record_id: Option<&'a str>,
    session_id: Option<&'a str>,
    request_id: Option<&'a str>,
    message_id: Option<&'a str>,
    timestamp_ms: u64,
    project: Option<&'a str>,
    provider: Option<&'a str>,
    model: Option<&'a str>,
    tokens: &'a TokenBuckets,
    source_cost_usd: Option<f64>,
    cost_authoritative: bool,
    dedupe_confidence: &'a str,
    conservative_undercount: bool,
    cache_chain_excluded: bool,
    sidechain: bool,
    permission_review: bool,
    source_order: u64,
}

impl<'a> CachedUsageEventRef<'a> {
    fn from_event(event: &'a UsageEvent) -> Self {
        Self {
            source_record_id: event.source_record_id.as_deref(),
            session_id: event.session_id.as_deref(),
            request_id: event.request_id.as_deref(),
            message_id: event.message_id.as_deref(),
            timestamp_ms: event.timestamp_ms,
            project: event.project.as_deref(),
            provider: event.provider.as_deref(),
            model: event.model.as_deref(),
            tokens: &event.tokens,
            source_cost_usd: event.source_cost_usd,
            cost_authoritative: event.cost_authoritative,
            dedupe_confidence: event.dedupe_confidence,
            conservative_undercount: event.conservative_undercount,
            cache_chain_excluded: event.cache_chain_excluded,
            sidechain: event.sidechain,
            permission_review: event.permission_review,
            source_order: event.source_order,
        }
    }
}

impl CachedUsageEvent {
    #[allow(dead_code)]
    fn from_event(event: &UsageEvent) -> Self {
        Self {
            source_record_id: event.source_record_id.clone(),
            session_id: event.session_id.clone(),
            request_id: event.request_id.clone(),
            message_id: event.message_id.clone(),
            timestamp_ms: event.timestamp_ms,
            project: event.project.clone(),
            provider: event.provider.clone(),
            model: event.model.clone(),
            tokens: event.tokens.clone(),
            source_cost_usd: event.source_cost_usd,
            cost_authoritative: event.cost_authoritative,
            dedupe_confidence: event.dedupe_confidence.to_string(),
            conservative_undercount: event.conservative_undercount,
            cache_chain_excluded: event.cache_chain_excluded,
            sidechain: event.sidechain,
            permission_review: event.permission_review,
            source_order: event.source_order,
        }
    }

    fn into_event(self, source: &'static str, source_path: Arc<str>) -> UsageEvent {
        UsageEvent {
            source,
            source_path,
            source_record_id: self.source_record_id,
            session_id: self.session_id,
            request_id: self.request_id,
            message_id: self.message_id,
            timestamp_ms: self.timestamp_ms,
            project: self.project,
            provider: self.provider,
            model: self.model,
            tokens: self.tokens,
            source_cost_usd: self.source_cost_usd,
            cost_authoritative: self.cost_authoritative,
            dedupe_confidence: match self.dedupe_confidence.as_str() {
                "exact" => "exact",
                "strong" => "strong",
                _ => "heuristic",
            },
            conservative_undercount: self.conservative_undercount,
            cache_chain_excluded: self.cache_chain_excluded,
            sidechain: self.sidechain,
            permission_review: self.permission_review,
            source_order: self.source_order,
        }
    }
}

struct UsageCache {
    connection: Connection,
}

struct CachedFileRow {
    size: u64,
    mtime_ns: i64,
    scanned_at_ms: i64,
    events_blob: Vec<u8>,
    deps: Vec<UsageFileDep>,
}

/// Row validity inputs without the event payload, for freshness checks.
struct CachedFileMeta {
    size: u64,
    mtime_ns: i64,
    scanned_at_ms: i64,
    deps: Vec<UsageFileDep>,
}

type FileParse = crate::sources::UsageParseOutput;
type UsageFileDep = crate::sources::UsageDependency;

/// Dependency representation written before native path bytes were persisted.
#[derive(Serialize, Deserialize)]
struct LegacyUsageFileDep {
    path: String,
    size: u64,
    mtime_ns: i64,
    exists: bool,
}

impl From<LegacyUsageFileDep> for UsageFileDep {
    fn from(dependency: LegacyUsageFileDep) -> Self {
        Self {
            native_path: dependency.path.as_bytes().to_vec(),
            path: dependency.path,
            size: dependency.size,
            mtime_ns: dependency.mtime_ns,
            exists: dependency.exists,
        }
    }
}

struct ParsedUsageFile {
    index: usize,
    path: PathBuf,
    size: u64,
    mtime_ns: i64,
    events: Vec<UsageEvent>,
    cacheable: bool,
    deps: Vec<UsageFileDep>,
}

impl UsageCache {
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        // Postcard encodes event fields positionally. Rebuild the disposable cache
        // when its event layout changes so old rows cannot decode with shifted fields.
        let event_format: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if event_format != 1 {
            connection
                .execute_batch("DROP TABLE IF EXISTS usage_file_cache; PRAGMA user_version = 1;")?;
        }
        // Drop pre-postcard cache tables and any schema missing a required column: the
        // JSON-era claude table, the pre-rename blob column, and the deps_blob column that
        // records cross-file dependencies. A missing column means an older layout, so the
        // table is rebuilt rather than migrated.
        let current_columns: i64 = connection.query_row(
            "SELECT count(*) FROM pragma_table_info('usage_file_cache')
             WHERE name IN ('events_blob', 'deps_blob')",
            [],
            |row| row.get(0),
        )?;
        if current_columns < 2 {
            connection.execute_batch("DROP TABLE IF EXISTS usage_file_cache;")?;
        }
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             DROP TABLE IF EXISTS claude_usage_file_cache;
             CREATE TABLE IF NOT EXISTS usage_file_cache (
                 source TEXT NOT NULL,
                 path TEXT NOT NULL,
                 parser_version INTEGER NOT NULL,
                 size INTEGER NOT NULL,
                 mtime_ns INTEGER NOT NULL,
                 scanned_at_ms INTEGER NOT NULL,
                 events_blob BLOB NOT NULL,
                 deps_blob BLOB NOT NULL,
                 PRIMARY KEY (source, path)
             );
             CREATE TABLE IF NOT EXISTS usage_facts (
                 source TEXT NOT NULL,
                 path TEXT NOT NULL,
                 source_order INTEGER NOT NULL,
                 ordinal INTEGER NOT NULL,
                 timestamp_ms INTEGER NOT NULL,
                 session_id TEXT,
                 project TEXT,
                 provider TEXT,
                 model TEXT,
                 source_record_id TEXT,
                 request_id TEXT,
                 message_id TEXT,
                 raw_input INTEGER NOT NULL DEFAULT 0,
                 uncached_input INTEGER NOT NULL DEFAULT 0,
                 cache_read INTEGER NOT NULL DEFAULT 0,
                 cache_write INTEGER NOT NULL DEFAULT 0,
                 cache_write_1h INTEGER NOT NULL DEFAULT 0,
                 output INTEGER NOT NULL DEFAULT 0,
                 reasoning INTEGER NOT NULL DEFAULT 0,
                 source_cost_usd REAL,
                 cost_authoritative INTEGER NOT NULL DEFAULT 0,
                 dedupe_confidence TEXT NOT NULL DEFAULT '',
                 conservative_undercount INTEGER NOT NULL DEFAULT 0,
                 cache_chain_excluded INTEGER NOT NULL DEFAULT 0,
                 sidechain INTEGER NOT NULL DEFAULT 0,
                 permission_review INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (source, path, source_order)
             );
             CREATE INDEX IF NOT EXISTS usage_facts_time
                 ON usage_facts(timestamp_ms);
             CREATE INDEX IF NOT EXISTS usage_facts_session
                 ON usage_facts(source, session_id);
             -- Per-source report order, served straight from the index with no
             -- sort step, including time-bounded ranges.
             CREATE INDEX IF NOT EXISTS usage_facts_source_time
                 ON usage_facts(ordinal, timestamp_ms, path, source_order);
             -- Facts freshness per source: fingerprint of the file set the facts
             -- were built from (full identity for plain logs, paths only for
             -- volatile databases, whose bytes are judged by reuse windows),
             -- plus the parser version that built them and the warnings that
             -- build reported. Written atomically with the facts themselves, so
             -- a match means the facts (and their warnings) are current.
              CREATE TABLE IF NOT EXISTS usage_fact_sync (
                  source TEXT PRIMARY KEY,
                  fingerprint TEXT NOT NULL,
                  parser_version INTEGER NOT NULL,
                  warnings TEXT NOT NULL DEFAULT '[]'
              );",
        )?;
        // Migrate sync rows written before warnings were recorded: absent column
        // reads as missing (forcing one legacy rebuild), but keep them readable.
        let sync_columns: i64 = connection.query_row(
            "SELECT count(*) FROM pragma_table_info('usage_fact_sync')
             WHERE name = 'warnings'",
            [],
            |row| row.get(0),
        )?;
        if sync_columns < 1 {
            connection.execute_batch(
                "ALTER TABLE usage_fact_sync ADD COLUMN warnings TEXT NOT NULL DEFAULT '[]';",
            )?;
        }
        // No VACUUM on the report path: it runs synchronously under the scan lock and
        // causes occasional large latency spikes. Run `vacuum_if_bloated` explicitly
        // from maintenance instead.
        Ok(Self { connection })
    }

    /// Chunked saves rewrite blob rows continuously and freed pages are never returned to
    /// the filesystem, so the cache file can grow to a large multiple of its live data.
    /// Explicit maintenance only: never call on the report path.
    #[allow(dead_code)]
    pub(crate) fn vacuum_if_bloated(path: &Path) -> Result<()> {
        let connection = Connection::open(path)?;
        let stats = (|| -> rusqlite::Result<(i64, i64, i64)> {
            let single = |pragma: &str| connection.query_row(pragma, [], |row| row.get(0));
            Ok((
                single("PRAGMA page_count")?,
                single("PRAGMA freelist_count")?,
                single("PRAGMA page_size")?,
            ))
        })();
        if let Ok((page_count, freelist_count, page_size)) = stats
            && freelist_count.saturating_mul(page_size) >= 64 * 1024 * 1024
            && freelist_count >= page_count / 4
        {
            let _ = connection.execute_batch("VACUUM;");
        }
        Ok(())
    }

    fn load_source(
        &self,
        source: &str,
        parser_version: i64,
    ) -> Result<HashMap<String, CachedFileRow>> {
        self.connection.execute(
            "DELETE FROM usage_file_cache WHERE source = ?1 AND parser_version != ?2",
            params![source, parser_version],
        )?;
        let mut cached = HashMap::new();
        let mut invalid_paths = Vec::new();
        {
            let mut statement = self.connection.prepare(
                "SELECT path, size, mtime_ns, scanned_at_ms, events_blob, deps_blob FROM usage_file_cache
                 WHERE source = ?1 AND parser_version = ?2",
            )?;
            let mut rows = statement.query(params![source, parser_version])?;
            while let Some(row) = rows.next()? {
                let path: String = row.get(0)?;
                let size = row.get::<_, i64>(1)? as u64;
                let mtime_ns = row.get::<_, i64>(2)?;
                let scanned_at_ms = row.get::<_, i64>(3)?;
                let Ok(events_blob) = row.get::<_, Vec<u8>>(4) else {
                    invalid_paths.push(path);
                    continue;
                };
                let Ok(deps_blob) = row.get::<_, Vec<u8>>(5) else {
                    invalid_paths.push(path);
                    continue;
                };
                let deps = postcard::from_bytes::<Vec<UsageFileDep>>(&deps_blob).or_else(|_| {
                    postcard::from_bytes::<Vec<LegacyUsageFileDep>>(&deps_blob).map(
                        |dependencies| dependencies.into_iter().map(UsageFileDep::from).collect(),
                    )
                });
                let Ok(deps) = deps else {
                    invalid_paths.push(path);
                    continue;
                };
                cached.insert(
                    path,
                    CachedFileRow {
                        size,
                        mtime_ns,
                        scanned_at_ms,
                        events_blob,
                        deps,
                    },
                );
            }
        }
        for path in invalid_paths {
            self.connection.execute(
                "DELETE FROM usage_file_cache WHERE source = ?1 AND path = ?2",
                params![source, path],
            )?;
        }
        Ok(cached)
    }

    /// Metadata-only row load for freshness checks: same version purge and dependency
    /// quarantine as `load_source`, but without transferring event payload blobs.
    /// A row is usable here exactly when the scan's stat phase would treat it as a hit;
    /// corrupt event blobs are invisible at this level and demote to reparse on refresh.
    fn load_source_meta(
        &self,
        source: &str,
        parser_version: i64,
    ) -> Result<HashMap<String, CachedFileMeta>> {
        self.connection.execute(
            "DELETE FROM usage_file_cache WHERE source = ?1 AND parser_version != ?2",
            params![source, parser_version],
        )?;
        let mut cached = HashMap::new();
        let mut invalid_paths = Vec::new();
        {
            let mut statement = self.connection.prepare(
                "SELECT path, size, mtime_ns, scanned_at_ms, deps_blob FROM usage_file_cache
                 WHERE source = ?1 AND parser_version = ?2",
            )?;
            let mut rows = statement.query(params![source, parser_version])?;
            while let Some(row) = rows.next()? {
                let path: String = row.get(0)?;
                let size = row.get::<_, i64>(1)? as u64;
                let mtime_ns = row.get::<_, i64>(2)?;
                let scanned_at_ms = row.get::<_, i64>(3)?;
                let Ok(deps_blob) = row.get::<_, Vec<u8>>(4) else {
                    invalid_paths.push(path);
                    continue;
                };
                let deps = postcard::from_bytes::<Vec<UsageFileDep>>(&deps_blob).or_else(|_| {
                    postcard::from_bytes::<Vec<LegacyUsageFileDep>>(&deps_blob).map(
                        |dependencies| dependencies.into_iter().map(UsageFileDep::from).collect(),
                    )
                });
                let Ok(deps) = deps else {
                    invalid_paths.push(path);
                    continue;
                };
                cached.insert(
                    path,
                    CachedFileMeta {
                        size,
                        mtime_ns,
                        scanned_at_ms,
                        deps,
                    },
                );
            }
        }
        for path in invalid_paths {
            self.connection.execute(
                "DELETE FROM usage_file_cache WHERE source = ?1 AND path = ?2",
                params![source, path],
            )?;
        }
        Ok(cached)
    }

    /// Replace one source partition's canonical facts from its freshly scanned,
    /// reconciled, and sorted events — including uncached ones (e.g. unresolved
    /// forks) that never reach the blob cache, so fact rows always describe the
    /// current assembly exactly. One transaction with the freshness fingerprint:
    /// a failed write leaves the previous partition intact, mirroring blob-save
    /// failure semantics.
    fn replace_partition_facts(
        &mut self,
        filter: SourceFilter,
        events: &[UsageEvent],
        fingerprint: &[(String, u64, i64)],
        warnings: &[String],
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM usage_facts WHERE source = ?1",
            params![filter.as_str()],
        )?;
        insert_fact_events(&transaction, filter, events)?;
        write_fact_sync(&transaction, filter, fingerprint, warnings)?;
        transaction.commit()?;
        Ok(())
    }

    /// Atomically replace the facts of individual files (changed, vanished, or
    /// previously uncached): delete their rows, insert the fresh events, and
    /// refresh the partition fingerprint. Hits keep their rows untouched.
    fn upsert_file_facts(
        &mut self,
        filter: SourceFilter,
        removed_paths: &[String],
        events: &[UsageEvent],
        fingerprint: &[(String, u64, i64)],
        warnings: &[String],
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        if !removed_paths.is_empty() {
            let mut delete =
                transaction.prepare("DELETE FROM usage_facts WHERE source = ?1 AND path = ?2")?;
            for path in removed_paths {
                delete.execute(params![filter.as_str(), path])?;
            }
        }
        insert_fact_events(&transaction, filter, events)?;
        write_fact_sync(&transaction, filter, fingerprint, warnings)?;
        transaction.commit()?;
        Ok(())
    }

    /// Freshness fingerprint recorded with a partition's facts, if any, plus the
    /// warnings that build reported. A missing row (or unreadable warnings JSON)
    /// means no usable facts: the caller falls back to a rebuild, which heals it.
    fn fact_sync(&self, source: &str) -> Result<Option<(String, i64, Vec<String>)>> {
        let row: Option<(String, i64, String)> = self
            .connection
            .query_row(
                "SELECT fingerprint, parser_version, warnings FROM usage_fact_sync WHERE source = ?1",
                params![source],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err::<_, anyhow::Error>(error.into()),
            })?;
        row.map(
            |(fingerprint, version, warnings)| -> Result<(String, i64, Vec<String>)> {
                let warnings = serde_json::from_str(&warnings).unwrap_or_default();
                Ok((fingerprint, version, warnings))
            },
        )
        .transpose()
    }
}

/// Insert canonical fact rows. The caller owns atomicity (same transaction as any
/// accompanying deletes and the sync row) and ordering (partition order for full
/// rewrites; per-file order is irrelevant since reads order by key). `connection`
/// accepts transactions through deref.
fn insert_fact_events(
    connection: &Connection,
    filter: SourceFilter,
    events: &[UsageEvent],
) -> Result<()> {
    let ordinal = source_ordinal(filter) as i64;
    let mut statement = connection.prepare(
        "INSERT INTO usage_facts(
             source, path, source_order, ordinal, timestamp_ms, session_id,
             project, provider, model, source_record_id, request_id, message_id,
             raw_input, uncached_input, cache_read, cache_write, cache_write_1h,
             output, reasoning, source_cost_usd, cost_authoritative,
             dedupe_confidence, conservative_undercount, cache_chain_excluded,
             sidechain, permission_review
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
             ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26
         )",
    )?;
    for event in events {
        debug_assert_eq!(event.source, filter.as_str());
        statement.execute(params![
            filter.as_str(),
            event.source_path.as_ref(),
            event.source_order as i64,
            ordinal,
            event.timestamp_ms as i64,
            event.session_id.as_deref(),
            event.project.as_deref(),
            event.provider.as_deref(),
            event.model.as_deref(),
            event.source_record_id.as_deref(),
            event.request_id.as_deref(),
            event.message_id.as_deref(),
            event.tokens.raw_input as i64,
            event.tokens.uncached_input as i64,
            event.tokens.cache_read as i64,
            event.tokens.cache_write as i64,
            event.tokens.cache_write_1h as i64,
            event.tokens.output as i64,
            event.tokens.reasoning as i64,
            event.source_cost_usd,
            i64::from(event.cost_authoritative),
            event.dedupe_confidence,
            i64::from(event.conservative_undercount),
            i64::from(event.cache_chain_excluded),
            i64::from(event.sidechain),
            i64::from(event.permission_review),
        ])?;
    }
    Ok(())
}

/// Record a partition's freshness fingerprint atomically with its facts.
/// Warnings accumulate deduplicated across upserts: a warning stays until a full
/// (legacy) rebuild recomputes them, so a fixed file's warning can linger — the
/// same direction as blob-save staleness, and self-healing on the next rebuild.
fn write_fact_sync(
    connection: &Connection,
    filter: SourceFilter,
    fingerprint: &[(String, u64, i64)],
    warnings: &[String],
) -> Result<()> {
    connection.execute(
        "INSERT INTO usage_fact_sync(source, fingerprint, parser_version, warnings)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(source) DO UPDATE SET
             fingerprint = excluded.fingerprint,
             parser_version = excluded.parser_version,
             warnings = excluded.warnings",
        params![
            filter.as_str(),
            fingerprint_files(&stable_triples(filter, fingerprint)),
            source_spec(filter).parser_version,
            serde_json::to_string(warnings).unwrap_or_else(|_| "[]".into()),
        ],
    )?;
    Ok(())
}

impl UsageCache {
    fn delete_stale(&mut self, source: &str, stale_paths: &[String]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        for path in stale_paths {
            transaction.execute(
                "DELETE FROM usage_file_cache WHERE source = ?1 AND path = ?2",
                params![source, path],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn save_batch(
        &mut self,
        source: &str,
        parser_version: i64,
        scanned_at_ms: i64,
        parsed: &[ParsedUsageFile],
    ) -> Result<()> {
        let prepared = parsed
            .iter()
            .filter(|file| file.cacheable)
            .map(|file| {
                let cached = file
                    .events
                    .iter()
                    .map(CachedUsageEventRef::from_event)
                    .collect::<Vec<_>>();
                Ok((
                    file.path.to_string_lossy().to_string(),
                    file.size,
                    file.mtime_ns,
                    postcard::to_stdvec(&cached)?,
                    postcard::to_stdvec(&file.deps)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let transaction = self.connection.transaction()?;
        for (path, size, mtime_ns, events_blob, deps_blob) in prepared {
            transaction.execute(
                "INSERT INTO usage_file_cache(
                     source, path, parser_version, size, mtime_ns, scanned_at_ms, events_blob, deps_blob
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(source, path) DO UPDATE SET
                     parser_version = excluded.parser_version,
                     size = excluded.size,
                     mtime_ns = excluded.mtime_ns,
                     scanned_at_ms = excluded.scanned_at_ms,
                     events_blob = excluded.events_blob,
                     deps_blob = excluded.deps_blob",
                params![
                    source,
                    path,
                    parser_version,
                    size as i64,
                    mtime_ns,
                    scanned_at_ms,
                    events_blob,
                    deps_blob
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn epoch_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[derive(Clone, Copy)]
struct SourceScan {
    source: &'static str,
    parser_version: i64,
    /// Returns how long cached rows for this path may be reused even if the file metadata
    /// changed, or `None` to always re-parse on change. Used for databases that are
    /// continuously rewritten while their application runs; plain log files must return
    /// `None` so appends are picked up immediately.
    volatile_reuse_ms: fn(&Path) -> Option<i64>,
}

/// Per-source inventory shared by scanning and freshness checks, so both agree on
/// which files, parser version, and volatility rule define a source.
struct SourceSpec {
    parser_version: i64,
    volatile_reuse_ms: fn(&Path) -> Option<i64>,
}

fn no_volatile_reuse(_path: &Path) -> Option<i64> {
    None
}

/// Only the databases are volatile; message JSON files are updated in place
/// while a response streams and must re-parse as soon as they change.
fn volatile_reuse_opencode(path: &Path) -> Option<i64> {
    (path.extension().and_then(|value| value.to_str()) == Some("db"))
        .then_some(VOLATILE_DB_REUSE_MS)
}

fn volatile_reuse_cursor(_path: &Path) -> Option<i64> {
    Some(VOLATILE_DB_REUSE_MS)
}

fn source_spec(filter: SourceFilter) -> SourceSpec {
    match filter {
        SourceFilter::Claude => SourceSpec {
            parser_version: crate::sources::claude::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Codex => SourceSpec {
            parser_version: crate::sources::codex::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Opencode => SourceSpec {
            parser_version: crate::sources::opencode::VERSIONS.usage,
            volatile_reuse_ms: volatile_reuse_opencode,
        },
        SourceFilter::Pi => SourceSpec {
            parser_version: crate::sources::pi::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Omp => SourceSpec {
            parser_version: crate::sources::omp::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::OpenClaw => SourceSpec {
            parser_version: crate::sources::openclaw::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Cursor => SourceSpec {
            parser_version: crate::sources::cursor::VERSIONS.usage,
            volatile_reuse_ms: volatile_reuse_cursor,
        },
        SourceFilter::Copilot => SourceSpec {
            parser_version: crate::sources::copilot::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Grok => SourceSpec {
            parser_version: crate::sources::grok::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Hermes => SourceSpec {
            parser_version: crate::sources::hermes::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Jcode => SourceSpec {
            parser_version: crate::sources::jcode::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Muse => SourceSpec {
            parser_version: crate::sources::muse::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        SourceFilter::Antigravity => SourceSpec {
            parser_version: crate::sources::antigravity::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
    }
}

/// The file listing each source scans. Freshness checks use the same listing so a
/// new, removed, or replaced file invalidates exactly when a rescan would reparse.
fn source_files(filter: SourceFilter) -> Vec<PathBuf> {
    match filter {
        SourceFilter::Claude => crate::sources::claude::usage_files(),
        SourceFilter::Codex => crate::sources::codex::discover_rollouts(None)
            .into_iter()
            .map(|file| file.path)
            .collect(),
        SourceFilter::Opencode => crate::sources::opencode::usage_files(),
        SourceFilter::Pi => crate::sources::pi::discover(None)
            .into_iter()
            .map(|file| file.path)
            .collect(),
        SourceFilter::Omp => crate::sources::omp::discover(None)
            .into_iter()
            .map(|file| file.path)
            .collect(),
        SourceFilter::OpenClaw => crate::sources::openclaw::discover()
            .into_iter()
            .map(|file| file.path)
            .collect(),
        SourceFilter::Cursor => crate::sources::cursor::usage_databases(),
        SourceFilter::Copilot => crate::sources::copilot::usage_files(),
        SourceFilter::Grok => crate::sources::grok::discover_sessions()
            .into_iter()
            .map(|file| file.path)
            .collect(),
        SourceFilter::Hermes => crate::sources::hermes::discover()
            .into_iter()
            .map(|file| file.path)
            .collect(),
        SourceFilter::Jcode => crate::sources::jcode::usage_files(),
        SourceFilter::Muse => crate::sources::muse::usage_files(),
        SourceFilter::Antigravity => crate::sources::antigravity::usage_files(),
    }
}

/// Ordinal of a source in scanner layout order. Stored per fact row so combined
/// reads reproduce the merged order exactly.
fn source_ordinal(filter: SourceFilter) -> usize {
    SCANNERS
        .iter()
        .position(|(candidate, _)| *candidate == filter)
        .expect("scanner for every source filter")
}

/// Sorted file identity triples for freshness fingerprints.
type FileFingerprint = Vec<(String, u64, i64)>;

/// Stable triples for freshness: full identity for plain log files, paths only
/// for volatile databases (their bytes are judged by reuse windows, not mtime).
/// Input comes sorted from discovery and stays sorted.
fn stable_triples(filter: SourceFilter, fingerprint: &[(String, u64, i64)]) -> FileFingerprint {
    let spec = source_spec(filter);
    fingerprint
        .iter()
        .map(|(path, size, mtime_ns)| {
            if (spec.volatile_reuse_ms)(Path::new(path)).is_some() {
                (path.clone(), 0, 0)
            } else {
                (path.clone(), *size, *mtime_ns)
            }
        })
        .collect()
}

/// FNV-1a 64-bit fingerprint over sorted file triples. Deterministic across
/// builds (unlike the default hasher), so fingerprints persist in SQLite.
fn fingerprint_files(triples: &[(String, u64, i64)]) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for (path, size, mtime_ns) in triples {
        for byte in path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
        for word in [*size, *mtime_ns as u64] {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(PRIME);
            }
        }
    }
    format!("{hash:016x}")
}

/// Scan `files` through the per-file cache: unchanged files are served from cached blobs
/// (decoded in parallel), changed or new files are re-parsed in parallel, and cache rows
/// for vanished files are dropped. Events are appended to `out` in `files` order.
fn scan_files_cached(
    scan: SourceScan,
    files: &[PathBuf],
    cache: Option<&mut UsageCache>,
    warnings: &mut Vec<String>,
    out: &mut Vec<UsageEvent>,
    parse: impl Fn(&Path) -> Result<FileParse> + Sync,
) {
    scan_files_cached_with(scan, files, cache, warnings, out, parse, |_| true);
}

/// Dependency metadata check with one stat per distinct path per scan. Each observation
/// is the live fingerprint, so rows recording different fingerprints for the same path
/// still compare correctly while sharing the single stat.
fn deps_observed_current(
    deps: &[UsageFileDep],
    observations: &mut HashMap<Vec<u8>, (u64, i64, bool)>,
) -> bool {
    deps.iter().all(|dep| {
        let observed = match observations.get(dep.native_path.as_slice()) {
            Some(&cached) => cached,
            None => {
                let fresh = dep.observed();
                observations.insert(dep.native_path.clone(), fresh);
                fresh
            }
        };
        (dep.size, dep.mtime_ns, dep.exists) == observed
    })
}

/// Like `scan_files_cached`, but with a source-specific validity predicate over a cached
/// row's recorded dependencies. `deps_current` runs in addition to each dependency's own
/// metadata check; a source uses it to invalidate cache hits on state that per-file metadata
/// cannot see — e.g. codex forks, whose baseline depends on the *set* of parent rollout
/// copies, so a newly appearing parent copy must invalidate the child even though every
/// already-recorded dependency is still unchanged.
#[allow(clippy::too_many_arguments)]
fn scan_files_cached_with(
    scan: SourceScan,
    files: &[PathBuf],
    cache: Option<&mut UsageCache>,
    warnings: &mut Vec<String>,
    out: &mut Vec<UsageEvent>,
    parse: impl Fn(&Path) -> Result<FileParse> + Sync,
    deps_current: impl Fn(&[UsageFileDep]) -> bool,
) {
    let SourceScan {
        source,
        parser_version,
        volatile_reuse_ms,
    } = scan;
    let now_ms = epoch_ms_now();
    let load_start = Instant::now();
    let mut rows = match cache.as_deref() {
        Some(cache) => match cache.load_source(source, parser_version) {
            Ok(rows) => rows,
            Err(error) => {
                warnings.push(format!("{source} usage cache read failed: {error:#}"));
                HashMap::new()
            }
        },
        None => HashMap::new(),
    };
    usage_timing(load_start, || {
        format!("{source} cache load ({} rows)", rows.len())
    });
    let stat_start = Instant::now();
    let mut slots: Vec<Option<Vec<UsageEvent>>> = (0..files.len()).map(|_| None).collect();
    let mut hits: Vec<(usize, String, Vec<u8>)> = Vec::new();
    let mut missing: Vec<(usize, PathBuf, (u64, i64))> = Vec::new();
    // One observation per distinct dependency path per scan: fork children sharing a
    // parent rollout stat it once instead of once per dependent file.
    let mut dep_observations: HashMap<Vec<u8>, (u64, i64, bool)> = HashMap::new();
    for (index, path) in files.iter().enumerate() {
        let metadata = match usage_file_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.push(format!(
                    "{source} usage file skipped ({}): {error:#}",
                    path.display()
                ));
                continue;
            }
        };
        let key = path.to_string_lossy().to_string();
        match rows.remove(&key) {
            // A dependency change (e.g. a fork's parent rollout was extended, or a new parent
            // copy appeared) invalidates the cached result even when the file itself is
            // unchanged, so it must re-parse.
            Some(row)
                if volatile_reuse_ms(path).map_or_else(
                    || (row.size, row.mtime_ns) == metadata,
                    |window| now_ms.saturating_sub(row.scanned_at_ms) < window,
                ) && deps_observed_current(&row.deps, &mut dep_observations)
                    && deps_current(&row.deps) =>
            {
                hits.push((index, key, row.events_blob));
            }
            _ => missing.push((index, path.clone(), metadata)),
        }
    }
    usage_timing(stat_start, || {
        format!("{source} stat ({} files)", files.len())
    });
    let decode_start = Instant::now();
    let hit_count = hits.len();
    let decoded = hits
        .into_par_iter()
        .map(|(index, key, blob)| {
            let source_path: Arc<str> = Arc::from(key.as_str());
            let events = postcard::from_bytes::<Vec<CachedUsageEvent>>(&blob).map(|events| {
                events
                    .into_iter()
                    .map(|event| event.into_event(source, source_path.clone()))
                    .collect::<Vec<_>>()
            });
            (index, key, events)
        })
        .collect::<Vec<_>>();
    for (index, key, events) in decoded {
        match events {
            Ok(events) => slots[index] = Some(events),
            // A corrupt cached blob demotes the file to a fresh parse.
            Err(_) => {
                let path = PathBuf::from(&key);
                match usage_file_metadata(&path) {
                    Ok(metadata) => missing.push((index, path, metadata)),
                    Err(error) => warnings.push(format!(
                        "{source} usage file skipped ({}): {error:#}",
                        path.display()
                    )),
                }
            }
        }
    }
    usage_timing(decode_start, || {
        format!("{source} decode ({hit_count} cached files)")
    });
    let mut cache = cache;
    let stale_paths: Vec<String> = rows.into_keys().collect();
    if let Some(cache) = cache.as_deref_mut()
        && !stale_paths.is_empty()
        && let Err(error) = cache.delete_stale(source, &stale_paths)
    {
        warnings.push(format!("{source} usage cache write failed: {error:#}"));
    }
    if !missing.is_empty() {
        publish_scan_progress(Some(UsageScanProgress {
            source,
            done: 0,
            total: missing.len(),
        }));
    }
    // Parse and persist in chunks so an interrupted cold scan keeps the chunks it finished;
    // the next scan resumes from there instead of re-parsing the whole source.
    let parse_start = Instant::now();
    let missing_count = missing.len();
    let mut save_warned = false;
    for chunk in missing.chunks(PARSE_SAVE_CHUNK) {
        let parsed = parse_missing_usage_files(source, chunk, warnings, &parse);
        // Unresolved-fork parses (cacheable == false) are excluded from persistence so a
        // later scan re-runs them once their fork parent is available; they still populate
        // `out`.
        if let Some(cache) = cache.as_deref_mut()
            && parsed.iter().any(|file| file.cacheable)
            && let Err(error) = cache.save_batch(source, parser_version, now_ms, &parsed)
            && !save_warned
        {
            save_warned = true;
            warnings.push(format!("{source} usage cache write failed: {error:#}"));
        }
        for file in parsed {
            slots[file.index] = Some(file.events);
        }
    }
    if missing_count > 0 {
        usage_timing(parse_start, || {
            format!("{source} parse ({missing_count} changed files)")
        });
    }
    // The decoded lengths are known; avoid repeatedly reallocating the large event buffer.
    out.reserve(slots.iter().flatten().map(Vec::len).sum());
    for events in slots.into_iter().flatten() {
        out.extend(events);
    }
}

fn usage_file_metadata(path: &Path) -> Result<(u64, i64)> {
    let metadata = path.metadata()?;
    let mtime_ns = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64;
    Ok((metadata.len(), mtime_ns))
}

fn scan_claude(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Claude);
    scan_files_cached(
        SourceScan {
            source: "claude",
            parser_version: crate::sources::claude::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::claude::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

/// Parse one file with its source's parser. Mirrors the parse expressions in the
/// `scan_*` functions (which stay on the legacy combined path); codex forks
/// resolve against `parents`, built from the same discovery as the split.
fn parse_source_file(
    filter: SourceFilter,
    path: &Path,
    parents: Option<&crate::sources::codex::UsageParentIndex>,
) -> Result<FileParse> {
    match filter {
        SourceFilter::Claude => {
            crate::sources::claude::parse_usage_file(path).map(FileParse::cacheable)
        }
        SourceFilter::Codex => crate::sources::codex::parse_usage_file(
            path,
            parents.expect("codex parsing needs its parent index"),
        ),
        SourceFilter::Opencode => {
            crate::sources::opencode::parse_usage_file(path).map(FileParse::cacheable)
        }
        SourceFilter::Pi => crate::sources::pi::parse_usage_file(path).map(FileParse::cacheable),
        SourceFilter::Omp => crate::sources::omp::parse_usage_file(path).map(FileParse::cacheable),
        SourceFilter::OpenClaw => {
            crate::sources::openclaw::parse_usage_file(path).map(FileParse::cacheable)
        }
        SourceFilter::Cursor => {
            crate::sources::cursor::parse_usage_database(path).map(FileParse::cacheable)
        }
        SourceFilter::Copilot => {
            crate::sources::copilot::parse_usage_file(path).map(FileParse::cacheable)
        }
        SourceFilter::Grok => {
            crate::sources::grok::parse_usage_file(path).map(FileParse::cacheable)
        }
        SourceFilter::Hermes => crate::sources::hermes::parse_usage_file(path),
        SourceFilter::Jcode => {
            crate::sources::jcode::parse_usage_file(path).map(FileParse::cacheable)
        }
        SourceFilter::Muse => {
            crate::sources::muse::parse_usage_file(path).map(FileParse::cacheable)
        }
        SourceFilter::Antigravity => {
            crate::sources::antigravity::parse_usage_file(path).map(FileParse::cacheable)
        }
    }
}

fn parse_missing_usage_files(
    source: &str,
    missing: &[(usize, PathBuf, (u64, i64))],
    warnings: &mut Vec<String>,
    parse: &(impl Fn(&Path) -> Result<FileParse> + Sync),
) -> Vec<ParsedUsageFile> {
    let outcomes = missing
        .par_iter()
        .map(|(index, path, metadata)| {
            let outcome = parse(path).map(|parsed| ParsedUsageFile {
                index: *index,
                path: path.clone(),
                size: metadata.0,
                mtime_ns: metadata.1,
                events: parsed.events,
                cacheable: parsed.cacheable,
                deps: parsed.deps,
            });
            bump_scan_progress();
            outcome
        })
        .collect::<Vec<_>>();
    let mut parsed = Vec::with_capacity(outcomes.len());
    for ((_, path, _), outcome) in missing.iter().zip(outcomes) {
        match outcome {
            Ok(file) => parsed.push(file),
            Err(error) => warnings.push(format!(
                "{source} usage file skipped ({}): {error:#}",
                path.display()
            )),
        }
    }
    parsed
}

fn scan_codex(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Codex);
    let parents = crate::sources::codex::UsageParentIndex::new(&files);
    scan_files_cached_with(
        SourceScan {
            source: "codex",
            parser_version: crate::sources::codex::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::codex::parse_usage_file(path, &parents),
        |deps| parents.deps_match_current_candidates(deps),
    );
    Ok(())
}

fn scan_pi(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Pi);
    scan_files_cached(
        SourceScan {
            source: "pi",
            parser_version: crate::sources::pi::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::pi::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

fn scan_omp(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Omp);
    scan_files_cached(
        SourceScan {
            source: "omp",
            parser_version: crate::sources::omp::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::omp::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

fn scan_openclaw(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::OpenClaw);
    scan_files_cached(
        SourceScan {
            source: "openclaw",
            parser_version: crate::sources::openclaw::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::openclaw::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

fn scan_opencode(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Opencode);
    scan_files_cached(
        SourceScan {
            source: "opencode",
            parser_version: crate::sources::opencode::VERSIONS.usage,
            volatile_reuse_ms: volatile_reuse_opencode,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::opencode::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

fn scan_cursor(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let databases = source_files(SourceFilter::Cursor);
    let start = out.len();
    scan_files_cached(
        SourceScan {
            source: "cursor",
            parser_version: crate::sources::cursor::VERSIONS.usage,
            volatile_reuse_ms: volatile_reuse_cursor,
        },
        &databases,
        cache,
        warnings,
        out,
        |path| crate::sources::cursor::parse_usage_database(path).map(FileParse::cacheable),
    );
    crate::sources::cursor::apply_projects(
        &mut out[start..],
        &crate::sources::cursor::project_by_session(),
    );
    Ok(())
}

fn scan_copilot(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Copilot);
    scan_files_cached(
        SourceScan {
            source: "copilot",
            parser_version: crate::sources::copilot::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::copilot::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

fn scan_grok(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Grok);
    scan_files_cached(
        SourceScan {
            source: "grok",
            parser_version: crate::sources::grok::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::grok::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

fn scan_hermes(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Hermes);
    scan_files_cached(
        SourceScan {
            source: "hermes",
            parser_version: crate::sources::hermes::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        crate::sources::hermes::parse_usage_file,
    );
    Ok(())
}

fn scan_jcode(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Jcode);
    scan_files_cached(
        SourceScan {
            source: "jcode",
            parser_version: crate::sources::jcode::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::jcode::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

fn scan_muse(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Muse);
    scan_files_cached(
        SourceScan {
            source: "muse",
            parser_version: crate::sources::muse::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::muse::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

fn scan_antigravity(
    out: &mut Vec<UsageEvent>,
    warnings: &mut Vec<String>,
    cache: Option<&mut UsageCache>,
) -> Result<()> {
    let files = source_files(SourceFilter::Antigravity);
    scan_files_cached(
        SourceScan {
            source: "antigravity",
            parser_version: crate::sources::antigravity::VERSIONS.usage,
            volatile_reuse_ms: no_volatile_reuse,
        },
        &files,
        cache,
        warnings,
        out,
        |path| crate::sources::antigravity::parse_usage_file(path).map(FileParse::cacheable),
    );
    Ok(())
}

// Rates are nano-USD per million tokens. The catalog is deliberately small and versioned:
// unknown models remain unpriced instead of silently inheriting a guessed family rate.
const PRICE_CATALOG_ID: &str = "official-api-prices-2026-07-15";

#[derive(Clone, Copy)]
struct Rates {
    input: u64,
    cache_read: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    output: u64,
}

const fn usd_per_million(value_milli_usd: u64) -> u64 {
    value_milli_usd * 1_000_000
}

#[allow(dead_code)]
pub(crate) fn event_cost_nanos<S: std::ops::Deref<Target = str>, P>(
    event: &UsageEventData<S, P>,
    mode: CostMode,
) -> Option<u64> {
    let mut cache = RateCache::default();
    event_cost_nanos_cached(event, mode, &mut cache)
}

fn event_cost_nanos_cached<S: std::ops::Deref<Target = str>, P>(
    event: &UsageEventData<S, P>,
    mode: CostMode,
    rates: &mut RateCache,
) -> Option<u64> {
    let source = event
        .source_cost_usd
        .filter(|value| value.is_finite() && *value >= 0.0)
        .and_then(|value| {
            let nanos = value * 1_000_000_000.0;
            (nanos <= u64::MAX as f64).then_some(nanos.round() as u64)
        });
    match mode {
        CostMode::Source => source,
        CostMode::Auto => source.or_else(|| {
            (!event.cost_authoritative)
                .then(|| calculated_cost_nanos_cached(event, rates))
                .flatten()
        }),
        CostMode::Reprice => calculated_cost_nanos_cached(event, rates),
    }
}

/// Caches catalog lookups per distinct raw provider/model pair so the event loop
/// avoids normalizing (allocating lowercase strings) on every event.
#[derive(Default)]
struct RateCache {
    entries: Vec<(Option<String>, String, Option<Rates>)>,
}

impl RateCache {
    fn rates_for(&mut self, provider: Option<&str>, model: &str) -> Option<Rates> {
        for (cached_provider, cached_model, rates) in &self.entries {
            if cached_provider.as_deref() == provider && cached_model == model {
                return *rates;
            }
        }
        let rates = rates_for(provider, model);
        self.entries
            .push((provider.map(str::to_owned), model.to_owned(), rates));
        rates
    }
}

#[allow(dead_code)]
fn calculated_cost_nanos<S: std::ops::Deref<Target = str>, P>(
    event: &UsageEventData<S, P>,
) -> Option<u64> {
    let mut cache = RateCache::default();
    calculated_cost_nanos_cached(event, &mut cache)
}

fn calculated_cost_nanos_cached<S: std::ops::Deref<Target = str>, P>(
    event: &UsageEventData<S, P>,
    cache: &mut RateCache,
) -> Option<u64> {
    let rates = cache.rates_for(event.provider.as_deref(), event.model.as_deref()?)?;
    let cache_write_1h = event.tokens.cache_write_1h.min(event.tokens.cache_write);
    let cache_write_5m = event.tokens.cache_write.saturating_sub(cache_write_1h);
    let total = (event.tokens.uncached_input as u128) * (rates.input as u128)
        + (event.tokens.cache_read as u128) * (rates.cache_read as u128)
        + (cache_write_5m as u128) * (rates.cache_write_5m as u128)
        + (cache_write_1h as u128) * (rates.cache_write_1h as u128)
        + (event.tokens.output as u128) * (rates.output as u128);
    // Rates are per million tokens. Reasoning is retained as an output subset and is not charged
    // a second time.
    u64::try_from(total / 1_000_000).ok()
}

fn rates_for(provider: Option<&str>, model: &str) -> Option<Rates> {
    let model = model.trim().to_ascii_lowercase();
    let provider = provider.unwrap_or("").trim().to_ascii_lowercase();
    let exact_or_snapshot = |base: &str| {
        model == base
            || model.strip_prefix(base).is_some_and(|suffix| {
                suffix.starts_with("-20")
                    && suffix[1..].chars().all(|c| c.is_ascii_digit() || c == '-')
            })
    };

    let openai = provider.is_empty()
        || provider.contains("openai")
        || provider.contains("codex")
        || provider.contains("github-copilot");
    if openai {
        if exact_or_snapshot("gpt-5.5") {
            return Some(openai_rates(5_000, 500, 30_000));
        }
        if exact_or_snapshot("gpt-5.4") {
            return Some(openai_rates(2_500, 250, 15_000));
        }
        if exact_or_snapshot("gpt-5.4-mini") {
            return Some(openai_rates(750, 75, 4_500));
        }
        if exact_or_snapshot("gpt-5.3-codex") || exact_or_snapshot("gpt-5.2-codex") {
            return Some(openai_rates(1_750, 175, 14_000));
        }
        if exact_or_snapshot("gpt-5-codex") || exact_or_snapshot("gpt-5") {
            return Some(openai_rates(1_250, 125, 10_000));
        }
        if exact_or_snapshot("gpt-4o") {
            return Some(openai_rates(2_500, 1_250, 10_000));
        }
        if exact_or_snapshot("gpt-4o-mini") {
            return Some(openai_rates(150, 75, 600));
        }
    }

    let anthropic = provider.is_empty() || provider.contains("anthropic");
    if anthropic {
        if [
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
        ]
        .iter()
        .any(|base| exact_or_snapshot(base))
        {
            return Some(claude_rates(5_000, 6_250, 10_000, 500, 25_000));
        }
        if exact_or_snapshot("claude-opus-4-1") || exact_or_snapshot("claude-opus-4") {
            return Some(claude_rates(15_000, 18_750, 30_000, 1_500, 75_000));
        }
        if exact_or_snapshot("claude-sonnet-5") {
            // Promotional rate valid on the catalog's 2026-07-15 effective date.
            return Some(claude_rates(2_000, 2_500, 4_000, 200, 10_000));
        }
        if ["claude-sonnet-4-6", "claude-sonnet-4-5", "claude-sonnet-4"]
            .iter()
            .any(|base| exact_or_snapshot(base))
        {
            return Some(claude_rates(3_000, 3_750, 6_000, 300, 15_000));
        }
        if exact_or_snapshot("claude-haiku-4-5") {
            return Some(claude_rates(1_000, 1_250, 2_000, 100, 5_000));
        }
    }
    None
}

fn openai_rates(input: u64, cached: u64, output: u64) -> Rates {
    Rates {
        input: usd_per_million(input),
        cache_read: usd_per_million(cached),
        cache_write_5m: usd_per_million(input),
        cache_write_1h: usd_per_million(input),
        output: usd_per_million(output),
    }
}

fn claude_rates(input: u64, write_5m: u64, write_1h: u64, read: u64, output: u64) -> Rates {
    Rates {
        input: usd_per_million(input),
        cache_read: usd_per_million(read),
        cache_write_5m: usd_per_million(write_5m),
        cache_write_1h: usd_per_million(write_1h),
        output: usd_per_million(output),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn activity_reports_progress_before_the_scan_completes() {
        let _scan_guard = super::USAGE_SCAN_LOCK.lock().unwrap();
        let (acknowledge, received) = std::sync::mpsc::channel();
        let mut updates = Vec::new();
        let value = super::with_usage_progress(
            move || {
                for done in [0, 1] {
                    super::publish_scan_progress(Some(super::UsageScanProgress {
                        source: "codex",
                        done,
                        total: 2,
                    }));
                    received
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .unwrap();
                }
                super::publish_scan_progress(None);
                42
            },
            |progress| {
                updates.push(progress.done);
                acknowledge.send(()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(updates, vec![0, 1]);
    }

    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn volatile_sqlite_usage_refreshes_held_open_wal_after_reuse_window() {
        for source in ["opencode", "cursor"] {
            let temp = tempfile::tempdir().unwrap();
            let database = temp.path().join(if source == "opencode" {
                "opencode.db"
            } else {
                "state.vscdb"
            });
            let writer = Connection::open(&database).unwrap();
            writer
                .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
                .unwrap();
            if source == "opencode" {
                writer
                    .execute_batch(
                        "CREATE TABLE message (id TEXT, session_id TEXT, data TEXT);
                     INSERT INTO message VALUES ('m', 's', '{\"tokens\":{\"input\":10}}');",
                    )
                    .unwrap();
            } else {
                writer
                    .execute_batch(
                        "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT);
                     INSERT INTO cursorDiskKV VALUES ('composerData:s',
                     '{\"generationUUID\":\"g\",\"inputTokens\":10}');",
                    )
                    .unwrap();
            }
            writer
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
            let metadata = usage_file_metadata(&database).unwrap();
            let mut cache = UsageCache::open(&temp.path().join("usage-cache.sqlite3")).unwrap();
            let run = |cache: &mut UsageCache| {
                let mut warnings = Vec::new();
                let mut events = Vec::new();
                scan_files_cached(
                    SourceScan {
                        source,
                        parser_version: 1,
                        volatile_reuse_ms: |_| Some(VOLATILE_DB_REUSE_MS),
                    },
                    std::slice::from_ref(&database),
                    Some(cache),
                    &mut warnings,
                    &mut events,
                    |path| {
                        if source == "opencode" {
                            crate::sources::opencode::parse_usage_file(path)
                        } else {
                            crate::sources::cursor::parse_usage_database(path)
                        }
                        .map(FileParse::cacheable)
                    },
                );
                assert!(warnings.is_empty(), "{source}: {warnings:?}");
                events
                    .iter()
                    .map(|event| event.tokens.additive_total())
                    .sum::<u64>()
            };
            assert_eq!(run(&mut cache), 10, "{source} cold read");
            if source == "opencode" {
                writer
                    .execute(
                        "UPDATE message SET data = '{\"tokens\":{\"input\":20}}'",
                        [],
                    )
                    .unwrap();
            } else {
                writer.execute("UPDATE cursorDiskKV SET value = '{\"generationUUID\":\"g\",\"inputTokens\":20}'", []).unwrap();
            }
            assert_eq!(usage_file_metadata(&database).unwrap(), metadata);
            assert_eq!(run(&mut cache), 10, "{source} preserves reuse window");
            cache
                .connection
                .execute("UPDATE usage_file_cache SET scanned_at_ms = 0", [])
                .unwrap();
            assert_eq!(run(&mut cache), 20, "{source} refreshes WAL after expiry");
            assert_eq!(usage_file_metadata(&database).unwrap(), metadata);
        }
    }

    #[test]
    fn usage_event_layout_change_rebuilds_cached_rows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&path).expect("open cache");
        cache
            .connection
            .execute_batch(
                "INSERT INTO usage_file_cache(source, path, parser_version, size, mtime_ns,
                 scanned_at_ms, events_blob, deps_blob)
             VALUES ('codex', '/tmp/review.jsonl', 1, 10, 20, 30, X'00', X'00');
             PRAGMA user_version = 0;",
            )
            .expect("seed older event layout");
        drop(cache);
        let rebuilt = UsageCache::open(&path).expect("rebuild cache");
        let rows: u64 = rebuilt
            .connection
            .query_row("SELECT count(*) FROM usage_file_cache", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(
            rebuilt
                .connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn usage_parser_version_change_invalidates_cached_rows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&path).expect("open cache");
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('claude', '/tmp/session.jsonl', 1, 10, 20, 30, ?1, ?2)",
                params![
                    postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap(),
                    postcard::to_stdvec(&Vec::<UsageFileDep>::new()).unwrap()
                ],
            )
            .expect("seed stale cache row");

        assert!(
            cache
                .load_source("claude", 2)
                .expect("load new parser version")
                .is_empty()
        );
        let rows: i64 = cache
            .connection
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'claude'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }
    #[test]
    fn malformed_dependency_rows_are_quarantined() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&path).expect("open cache");
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('omp', '/tmp/session.jsonl', 1, 10, 20, 30, ?1, ?2)",
                params![
                    postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap(),
                    vec![0xff_u8]
                ],
            )
            .expect("seed malformed cache row");

        assert!(cache.load_source("omp", 1).expect("load cache").is_empty());
        let rows: i64 = cache
            .connection
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'omp'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn malformed_event_rows_are_quarantined() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&path).expect("open cache");
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('omp', '/tmp/session.jsonl', 1, 10, 20, 30, ?1, ?2)",
                params![
                    "not a blob",
                    postcard::to_stdvec(&Vec::<UsageFileDep>::new()).unwrap()
                ],
            )
            .expect("seed malformed cache row");

        assert!(cache.load_source("omp", 1).expect("load cache").is_empty());
        let rows: i64 = cache
            .connection
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'omp'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn hermes_disjoint_usage_parser_version_is_newer_than_repair_six() {
        assert_eq!(crate::sources::hermes::VERSIONS.usage, 7);
    }

    #[test]
    fn hermes_parser_version_change_reparses_a_repair_six_cache_row() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("state.db");
        let conn = Connection::open(&db_path).expect("create db");
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, model TEXT, started_at INTEGER, input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER, billing_provider TEXT, estimated_cost_usd REAL, cwd TEXT, git_repo_root TEXT, profile_name TEXT);",
        )
        .expect("create sessions");
        drop(conn);
        let cache_path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&cache_path).expect("open cache");
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('hermes', ?1, 6, ?2, ?3, 30, ?4, ?5)",
                params![
                    db_path.to_string_lossy(),
                    fs::metadata(&db_path).unwrap().len() as i64,
                    usage_file_metadata(&db_path).unwrap().1,
                    postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap(),
                    postcard::to_stdvec(&Vec::<UsageFileDep>::new()).unwrap()
                ],
            )
            .expect("seed repair-six row");
        drop(cache);

        let mut cache = UsageCache::open(&cache_path).expect("reopen cache");
        let mut warnings = Vec::new();
        let mut events = Vec::new();
        let parses = AtomicUsize::new(0);
        scan_files_cached(
            SourceScan {
                source: "hermes",
                parser_version: crate::sources::hermes::VERSIONS.usage,
                volatile_reuse_ms: no_volatile_reuse,
            },
            std::slice::from_ref(&db_path),
            Some(&mut cache),
            &mut warnings,
            &mut events,
            |path| {
                parses.fetch_add(1, Ordering::SeqCst);
                crate::sources::hermes::parse_usage_file(path)
            },
        );
        assert_eq!(parses.load(Ordering::SeqCst), 1);
        assert!(warnings.is_empty());
        let version: i64 = cache
            .connection
            .query_row(
                "SELECT parser_version FROM usage_file_cache WHERE source = 'hermes'",
                [],
                |row| row.get(0),
            )
            .expect("reparsed row");
        assert_eq!(version, 7);
    }

    #[test]
    fn previous_dependency_postcard_format_loads_with_native_path_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source_path = temp.path().join("rollout.jsonl");
        let dependency_path = temp.path().join("parent.jsonl");
        fs::write(&source_path, "source").expect("write source");
        fs::write(&dependency_path, "dependency").expect("write dependency");
        let cache_path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&cache_path).expect("open cache");
        let source_metadata = usage_file_metadata(&source_path).expect("source metadata");
        let dependency_metadata = usage_file_metadata(&dependency_path).expect("dep metadata");
        let source_key = source_path.to_string_lossy().to_string();
        let dependency_key = dependency_path.to_string_lossy().to_string();
        let legacy = vec![LegacyUsageFileDep {
            path: dependency_key.clone(),
            size: dependency_metadata.0,
            mtime_ns: dependency_metadata.1,
            exists: true,
        }];
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('codex', ?1, ?2, ?3, ?4, 30, ?5, ?6)",
                params![
                    source_key,
                    crate::sources::codex::VERSIONS.usage,
                    source_metadata.0 as i64,
                    source_metadata.1,
                    postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap(),
                    postcard::to_stdvec(&legacy).unwrap()
                ],
            )
            .expect("seed previous dependency format");

        let loaded = cache
            .load_source("codex", crate::sources::codex::VERSIONS.usage)
            .expect("load previous dependency format");
        let dependency = &loaded[&source_key].deps[0];
        assert_eq!(dependency.native_path, dependency_key.as_bytes());
        assert!(dependency.is_current());
    }

    #[derive(Serialize)]
    struct LegacyUsageDependency {
        path: String,
        size: u64,
        mtime_ns: i64,
    }

    #[test]
    fn legacy_nonempty_dependency_blob_cannot_become_a_dependency_free_hit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source_path = temp.path().join("rollout.jsonl");
        fs::write(&source_path, "source").expect("write source");
        let cache_path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&cache_path).expect("open cache");
        let metadata = usage_file_metadata(&source_path).expect("metadata");
        let legacy = vec![LegacyUsageDependency {
            path: temp
                .path()
                .join("parent.jsonl")
                .to_string_lossy()
                .to_string(),
            size: 12,
            mtime_ns: 34,
        }];
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('codex', ?1, ?2, ?3, ?4, 30, ?5, ?6)",
                params![
                    source_path.to_string_lossy(),
                    crate::sources::codex::VERSIONS.usage,
                    metadata.0 as i64,
                    metadata.1,
                    postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap(),
                    postcard::to_stdvec(&legacy).unwrap()
                ],
            )
            .expect("seed legacy row");
        drop(cache);

        let mut cache = UsageCache::open(&cache_path).expect("reopen cache");
        let mut warnings = Vec::new();
        let mut events = Vec::new();
        let parses = AtomicUsize::new(0);
        scan_files_cached(
            SourceScan {
                source: "codex",
                parser_version: crate::sources::codex::VERSIONS.usage,
                volatile_reuse_ms: no_volatile_reuse,
            },
            std::slice::from_ref(&source_path),
            Some(&mut cache),
            &mut warnings,
            &mut events,
            |_| {
                parses.fetch_add(1, Ordering::SeqCst);
                Ok(FileParse::cacheable(Vec::new()))
            },
        );
        assert_eq!(parses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn corrupted_dependency_blob_forces_a_reparse() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source_path = temp.path().join("rollout.jsonl");
        fs::write(&source_path, "source").expect("write source");
        let cache_path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&cache_path).expect("open cache");
        let metadata = usage_file_metadata(&source_path).expect("metadata");
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('codex', ?1, ?2, ?3, ?4, 30, ?5, ?6)",
                params![
                    source_path.to_string_lossy(),
                    crate::sources::codex::VERSIONS.usage,
                    metadata.0 as i64,
                    metadata.1,
                    postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap(),
                    vec![0xff_u8, 0x00_u8]
                ],
            )
            .expect("seed corrupt row");
        drop(cache);

        let mut cache = UsageCache::open(&cache_path).expect("reopen cache");
        let mut warnings = Vec::new();
        let mut events = Vec::new();
        let parses = AtomicUsize::new(0);
        scan_files_cached(
            SourceScan {
                source: "codex",
                parser_version: crate::sources::codex::VERSIONS.usage,
                volatile_reuse_ms: no_volatile_reuse,
            },
            std::slice::from_ref(&source_path),
            Some(&mut cache),
            &mut warnings,
            &mut events,
            |_| {
                parses.fetch_add(1, Ordering::SeqCst);
                Ok(FileParse::cacheable(Vec::new()))
            },
        );
        assert_eq!(parses.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_dependency_row_does_not_discard_valid_cache_rows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let valid_path = temp.path().join("valid.jsonl");
        fs::write(&valid_path, "valid").expect("write valid source");
        let vanished_path = temp.path().join("vanished.jsonl");
        let malformed_path = temp.path().join("malformed.jsonl");
        let cache_path = temp.path().join("usage-cache.sqlite3");
        let cache = UsageCache::open(&cache_path).expect("open cache");
        let metadata = usage_file_metadata(&valid_path).expect("metadata");
        let empty_events = postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap();
        let empty_deps = postcard::to_stdvec(&Vec::<UsageFileDep>::new()).unwrap();
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('codex', ?1, ?2, ?3, ?4, 30, ?5, ?6)",
                params![
                    valid_path.to_string_lossy(),
                    crate::sources::codex::VERSIONS.usage,
                    metadata.0 as i64,
                    metadata.1,
                    empty_events,
                    empty_deps
                ],
            )
            .expect("seed valid row");
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('codex', ?1, ?2, 0, 0, 30, ?3, ?4)",
                params![
                    vanished_path.to_string_lossy(),
                    crate::sources::codex::VERSIONS.usage,
                    postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap(),
                    vec![0xff_u8, 0x00_u8]
                ],
            )
            .expect("seed invalid row");
        cache
            .connection
            .execute(
                "INSERT INTO usage_file_cache(
                    source, path, parser_version, size, mtime_ns, scanned_at_ms,
                    events_blob, deps_blob
                 ) VALUES ('codex', ?1, ?2, 0, 0, 30, ?3, 7)",
                params![
                    malformed_path.to_string_lossy(),
                    crate::sources::codex::VERSIONS.usage,
                    postcard::to_stdvec(&Vec::<CachedUsageEvent>::new()).unwrap(),
                ],
            )
            .expect("seed malformed dependency type row");

        let mut cache = cache;
        let mut warnings = Vec::new();
        let mut events = Vec::new();
        let parses = AtomicUsize::new(0);
        scan_files_cached(
            SourceScan {
                source: "codex",
                parser_version: crate::sources::codex::VERSIONS.usage,
                volatile_reuse_ms: no_volatile_reuse,
            },
            std::slice::from_ref(&valid_path),
            Some(&mut cache),
            &mut warnings,
            &mut events,
            |_| {
                parses.fetch_add(1, Ordering::SeqCst);
                Ok(FileParse::cacheable(Vec::new()))
            },
        );

        assert_eq!(parses.load(Ordering::SeqCst), 0);
        assert!(
            cache
                .load_source("codex", crate::sources::codex::VERSIONS.usage)
                .is_ok()
        );
        let invalid_rows: i64 = cache
            .connection
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'codex' AND path = ?1",
                [vanished_path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invalid_rows, 0);
        let malformed_rows: i64 = cache
            .connection
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'codex' AND path = ?1",
                [malformed_path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(malformed_rows, 0);
    }

    #[test]
    fn hermes_wal_changes_invalidate_but_shm_changes_do_not() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("state.db");
        let conn = Connection::open(&db_path).expect("create db");
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, model TEXT, started_at INTEGER, input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER, billing_provider TEXT, estimated_cost_usd REAL, cwd TEXT, git_repo_root TEXT, profile_name TEXT);",
        )
        .expect("create sessions");
        drop(conn);
        let wal = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let shm = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        fs::write(&wal, "wal-1").expect("write wal");
        fs::write(&shm, "shm-1").expect("write shm");
        let cache_path = temp.path().join("usage-cache.sqlite3");
        let mut cache = UsageCache::open(&cache_path).expect("open cache");
        let mut warnings = Vec::new();
        let mut events = Vec::new();
        let parses = AtomicUsize::new(0);
        let scan =
            |cache: &mut UsageCache, warnings: &mut Vec<String>, events: &mut Vec<UsageEvent>| {
                scan_files_cached(
                    SourceScan {
                        source: "hermes",
                        parser_version: crate::sources::hermes::VERSIONS.usage,
                        volatile_reuse_ms: no_volatile_reuse,
                    },
                    std::slice::from_ref(&db_path),
                    Some(cache),
                    warnings,
                    events,
                    |path| {
                        parses.fetch_add(1, Ordering::SeqCst);
                        crate::sources::hermes::parse_usage_file(path)
                    },
                );
            };
        scan(&mut cache, &mut warnings, &mut events);
        fs::write(&shm, "shm-2").expect("change shm");
        scan(&mut cache, &mut warnings, &mut events);
        assert_eq!(parses.load(Ordering::SeqCst), 1);
        fs::write(&wal, "wal-2").expect("change wal");
        scan(&mut cache, &mut warnings, &mut events);
        assert_eq!(parses.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn openai_cached_input_is_a_subset() {
        let tokens = TokenBuckets::codex(100, 80, 10, 4);
        assert_eq!(tokens.uncached_input, 20);
        assert_eq!(tokens.cache_read, 80);
        assert_eq!(tokens.additive_total(), 110);
    }

    pub(super) fn cache_event(
        session: &str,
        timestamp_ms: u64,
        model: &str,
        uncached: u64,
        read: u64,
        write: u64,
    ) -> UsageEvent {
        UsageEvent {
            source: "claude",
            source_path: Arc::from("log.jsonl"),
            source_record_id: None,
            session_id: Some(session.to_string()),
            request_id: None,
            message_id: None,
            timestamp_ms,
            project: None,
            provider: Some("anthropic".to_string()),
            model: Some(model.to_string()),
            tokens: TokenBuckets {
                raw_input: uncached,
                uncached_input: uncached,
                cache_read: read,
                cache_write: write,
                cache_write_1h: 0,
                output: 10,
                reasoning: 0,
            },
            source_cost_usd: None,
            cost_authoritative: false,
            dedupe_confidence: "exact",
            conservative_undercount: false,
            cache_chain_excluded: false,
            sidechain: false,
            permission_review: false,
            source_order: 0,
        }
    }

    fn waste_for(events: &[UsageEvent]) -> Option<CacheWaste> {
        let assembly = UsageAssembly::new(events.to_vec(), None);
        compute_cache_waste((0..assembly.len()).map(|index| assembly.view(index))).remove("claude")
    }

    #[test]
    fn index_sort_preserves_stable_order_including_equal_keys() {
        for len in [0, 1, 2, 7, 128, 4097] {
            let mut events: Vec<_> = (0..len)
                .map(|index| {
                    let mut event =
                        cache_event("session", ((index * 17) % 23) as u64, "model", 10, 0, 0);
                    event.source_path = Arc::from(format!("path-{}", (index * 7) % 3));
                    event.source_order = (index % 5) as u64;
                    event.source_record_id = Some(index.to_string());
                    event
                })
                .collect();
            let mut expected = events.clone();
            expected.sort_by(|a, b| {
                (a.timestamp_ms, &a.source_path, a.source_order).cmp(&(
                    b.timestamp_ms,
                    &b.source_path,
                    b.source_order,
                ))
            });
            sort_usage_events(&mut events);
            assert_eq!(
                serde_json::to_value(events).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[test]
    fn merged_order_matches_combined_sort() {
        // Interleaved timestamps with cross-partition ties on (timestamp, path,
        // order): the ordinal tiebreak must reproduce the combined stable sort.
        let event = |timestamp_ms: u64, source_order: u64| {
            let mut event = cache_event("session", timestamp_ms, "model", 10, 0, 0);
            event.source_order = source_order;
            event
        };
        // Partitions arrive sorted, exactly as snapshot refreshes leave them.
        let mut a = vec![event(5, 2), event(5, 0), event(3, 1)];
        let mut b = vec![event(5, 0), event(1, 3)];
        sort_usage_events(&mut a);
        sort_usage_events(&mut b);
        let parts = vec![
            Arc::new(UsageAssembly::new(a.clone(), None)),
            Arc::new(UsageAssembly::Owned(b.clone())),
        ];
        let order = build_merged_order(&parts);
        let mut combined = [a, b].concat();
        sort_usage_events(&mut combined);
        assert_eq!(order.len(), combined.len());
        for (position, pos) in order.iter().enumerate() {
            let view = parts[pos.part as usize].view(pos.index as usize);
            let expected = &combined[position];
            assert_eq!(
                (
                    view.timestamp_ms,
                    view.source_path,
                    view.source_order,
                    view.tokens.additive_total()
                ),
                (
                    expected.timestamp_ms,
                    expected.source_path.as_ref(),
                    expected.source_order,
                    expected.tokens.additive_total()
                ),
                "merged position {position} (part {}, index {})",
                pos.part,
                pos.index
            );
        }
        // Empty partitions merge to the other partition's order, unchanged.
        let empty = vec![
            Arc::new(UsageAssembly::new(Vec::new(), None)),
            parts[1].clone(),
        ];
        let order = build_merged_order(&empty);
        assert!(order.iter().all(|pos| pos.part == 1));
        assert_eq!(order.len(), parts[1].len());
    }

    #[test]
    fn cache_idle_gap_miss_is_counted_and_attributed() {
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000),
            cache_event("s", 10 * 60 * 1000, "claude-sonnet-4-6", 0, 0, 100_500),
        ];
        let waste = waste_for(&events).expect("miss counted");
        assert_eq!(waste.miss_count, 1);
        assert_eq!(waste.missed_tokens, 100_000);
        assert_eq!(waste.idle_misses, 1);
        assert_eq!(waste.model_switch_misses, 0);
        // 100k tokens re-billed at the 5m cache-write rate ($3.75/M) vs read ($0.30/M).
        assert!((waste.missed_cost_usd - 0.345).abs() < 1e-9);
    }

    #[test]
    fn cache_warm_hit_is_not_a_miss() {
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000),
            cache_event("s", 60_000, "claude-sonnet-4-6", 0, 100_000, 500),
        ];
        assert!(waste_for(&events).is_none());
    }

    #[test]
    fn cache_model_switch_miss_is_attributed_to_the_switch() {
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000),
            cache_event("s", 60_000, "claude-opus-4-8", 0, 0, 100_000),
        ];
        let waste = waste_for(&events).expect("miss counted");
        assert_eq!(waste.miss_count, 1);
        assert_eq!(waste.model_switch_misses, 1);
        assert_eq!(waste.idle_misses, 0);
    }

    #[test]
    fn cache_prompt_shrink_is_treated_as_context_reset() {
        // A prompt below half of its predecessor stands in for compaction/clear: the first
        // post-shrink request is exempt, and the chain rebases onto the shrunk prompt.
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000),
            cache_event("s", 60_000, "claude-sonnet-4-6", 0, 0, 20_000),
            cache_event("s", 120_000, "claude-sonnet-4-6", 0, 0, 20_500),
        ];
        let waste = waste_for(&events).expect("post-reset miss counted");
        assert_eq!(waste.miss_count, 1);
        assert_eq!(waste.missed_tokens, 20_000);
    }

    #[test]
    fn cache_miss_below_noise_floor_is_ignored() {
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 10_000),
            cache_event("s", 60_000, "claude-sonnet-4-6", 0, 9_500, 1_000),
        ];
        assert!(waste_for(&events).is_none());
    }

    #[test]
    fn cache_first_write_after_uncached_prompts_is_not_a_miss() {
        // The chain's first cache write creates the cache; the earlier uncached prompt
        // could not have been served from it. Once the chain has reported cache activity,
        // a later write-only turn is a genuine full miss.
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 50_000, 0, 0),
            cache_event("s", 60_000, "claude-sonnet-4-6", 0, 0, 52_000),
            cache_event("s", 120_000, "claude-sonnet-4-6", 0, 0, 53_000),
        ];
        let waste = waste_for(&events).expect("post-write miss counted");
        assert_eq!(waste.miss_count, 1);
        assert_eq!(waste.missed_tokens, 52_000);
    }

    #[test]
    fn cache_never_reported_provider_is_not_counted() {
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 50_000, 0, 0),
            cache_event("s", 60_000, "claude-sonnet-4-6", 50_500, 0, 0),
        ];
        assert!(waste_for(&events).is_none());
    }

    #[test]
    fn cache_read_only_provider_total_miss_counts_after_reported_cache() {
        // OpenAI-style: reads reported, writes not. Once cache activity has been seen, a
        // zero-cache request is a total miss.
        let mut first = cache_event("s", 0, "gpt-5.4", 10_000, 40_000, 0);
        first.provider = Some("openai".to_string());
        let mut second = cache_event("s", 60_000, "gpt-5.4", 50_500, 0, 0);
        second.provider = Some("openai".to_string());
        let waste = waste_for(&[first, second]).expect("total miss counted");
        assert_eq!(waste.miss_count, 1);
        assert_eq!(waste.missed_tokens, 50_000);
        // 50k tokens at gpt-5.4 input ($2.50/M) vs cached ($0.25/M).
        assert!((waste.missed_cost_usd - 0.1125).abs() < 1e-9);
    }

    #[test]
    fn cache_sidechain_events_are_excluded_from_chains() {
        let mut sidechain = cache_event("s", 30_000, "claude-sonnet-4-6", 0, 0, 5_000);
        sidechain.sidechain = true;
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000),
            sidechain,
            cache_event("s", 60_000, "claude-sonnet-4-6", 0, 100_000, 500),
        ];
        assert!(waste_for(&events).is_none());
    }

    #[test]
    fn hermes_aggregate_rows_are_excluded_from_cache_chains() {
        let mut first = cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000);
        first.source = "hermes";
        first.source_path = Arc::from("hermes.db");
        first.cache_chain_excluded = true;
        let mut second = cache_event("s", 60_000, "claude-sonnet-4-6", 0, 0, 100_500);
        second.source = "hermes";
        second.source_path = Arc::from("hermes.db");
        second.cache_chain_excluded = true;
        let assembly = UsageAssembly::new(vec![first, second], None);
        let waste = compute_cache_waste([assembly.view(0), assembly.view(1)]);
        assert!(!waste.contains_key("hermes"));
    }

    #[test]
    fn cache_conservative_events_break_the_chain() {
        // Clamped dedupe deltas do not describe a real request's prompt; neither the
        // conservative event nor its successor may be counted against the chain.
        let mut clamped = cache_event("s", 30_000, "claude-sonnet-4-6", 0, 0, 40_000);
        clamped.conservative_undercount = true;
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000),
            clamped,
            cache_event("s", 60_000, "claude-sonnet-4-6", 0, 0, 100_500),
        ];
        assert!(waste_for(&events).is_none());
    }

    #[test]
    fn cache_sessions_chain_independently() {
        let events = vec![
            cache_event("a", 0, "claude-sonnet-4-6", 0, 0, 100_000),
            cache_event("b", 60_000, "claude-sonnet-4-6", 0, 0, 100_000),
        ];
        assert!(waste_for(&events).is_none());
    }

    #[test]
    fn cache_parallel_threads_sharing_a_session_chain_per_file() {
        // Codex spawned/resumed threads share a session id across rollout files; comparing
        // across files fabricates misses.
        let mut thread = cache_event("s", 30_000, "claude-sonnet-4-6", 0, 0, 90_000);
        thread.source_path = Arc::from("thread.jsonl");
        let events = vec![
            cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000),
            thread,
            cache_event("s", 60_000, "claude-sonnet-4-6", 0, 100_000, 500),
        ];
        assert!(waste_for(&events).is_none());
    }

    #[test]
    fn cache_opencode_chains_across_per_message_files() {
        let mut first = cache_event("s", 0, "claude-sonnet-4-6", 0, 0, 100_000);
        first.source = "opencode";
        first.source_path = Arc::from("msg-1.json");
        let mut second = cache_event("s", 10 * 60 * 1000, "claude-sonnet-4-6", 0, 0, 100_500);
        second.source = "opencode";
        second.source_path = Arc::from("msg-2.json");
        let assembly = UsageAssembly::new(vec![first, second], None);
        let waste = compute_cache_waste([assembly.view(0), assembly.view(1)])
            .remove("opencode")
            .expect("miss counted");
        assert_eq!(waste.miss_count, 1);
        assert_eq!(waste.idle_misses, 1);
    }

    #[test]
    fn claude_scanner_caches_normalized_usage_by_file_metadata() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        let transcript = projects.join("session.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","sessionId":"session","requestId":"request","timestamp":"2026-07-03T01:02:05Z","cwd":"/repo/memex","costUSD":"invalid optional value","message":{"id":"message","model":"claude-sonnet-4-6","content":[{"type":"text","text":"ignored payload"}],"usage":{"inputTokens":10,"cacheReadInputTokens":2,"cacheCreationInputTokens":3,"outputTokens":4,"cache_creation":{"ephemeral_1h_input_tokens":1}}}}"#,
                "\n"
            ),
        )
        .expect("write transcript");
        let cache_path = tmp.path().join("usage-cache.sqlite3");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            include_events: true,
            cache_path: Some(cache_path.clone()),
            ..UsageQuery::default()
        };

        let cold = scan_usage(&query).expect("cold scan");
        let warm = scan_usage(&query).expect("warm scan");
        let cache = Connection::open(cache_path).expect("open cache");
        let cached_files: u64 = cache
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'claude'",
                [],
                |row| row.get(0),
            )
            .expect("count cached files");

        assert_eq!(cold.events, 1);
        assert_eq!(cold.details[0].tokens.total(), 19);
        assert_eq!(cold.details[0].tokens.cache_write_1h, 1);
        assert_eq!(cold.details[0].dedupe_confidence, "exact");
        assert_eq!(warm.total_tokens, cold.total_tokens);
        assert_eq!(cached_files, 1);
    }

    #[test]
    fn claude_warm_cache_reconciles_old_parents_before_since_filter() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        let subagents = projects.join("subagents");
        std::fs::create_dir_all(&subagents).expect("create projects");
        std::fs::write(
            projects.join("parent.jsonl"),
            concat!(
                r#"{"type":"assistant","sessionId":"parent","requestId":"parent-request","timestamp":1000,"cwd":"/repo/memex","message":{"id":"shared-message","model":"claude-sonnet-4-6","usage":{"inputTokens":10}}}"#,
                "\n"
            ),
        )
        .expect("write parent transcript");
        std::fs::write(
            subagents.join("agent.jsonl"),
            concat!(
                r#"{"type":"assistant","sessionId":"agent","requestId":"sidechain-request","timestamp":3000,"cwd":"/repo/memex","isSidechain":true,"message":{"id":"shared-message","model":"claude-sonnet-4-6","usage":{"inputTokens":10}}}"#,
                "\n"
            ),
        )
        .expect("write sidechain transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            since_ms: Some(2_000_000),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        let cold = scan_usage(&query).expect("cold scan");
        let cache = Connection::open(query.cache_path.as_ref().expect("cache path"))
            .expect("open usage cache");
        let cached_files: u64 = cache
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'claude'",
                [],
                |row| row.get(0),
            )
            .expect("count cached files");
        let warm = scan_usage(&query).expect("warm scan");

        assert_eq!(cold.events, 0);
        assert_eq!(cached_files, 2);
        assert_eq!(warm.events, cold.events);
        assert_eq!(warm.total_tokens, 0);
    }

    #[test]
    fn claude_lines_with_both_session_field_spellings_are_counted() {
        // Claude Code 2.1.210+ writes `session_id` AND `sessionId` (and can do the same
        // for request ids) on one line; a duplicate-field parse error must not drop it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript = tmp.path().join("session.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","session_id":"ses-1","sessionId":"ses-1","requestId":"req-1","request_id":"req-1","timestamp":1000,"cwd":"/repo/memex","message":{"id":"msg-1","model":"claude-opus-4-8","usage":{"input_tokens":2,"cache_read_input_tokens":52196,"cache_creation_input_tokens":558,"output_tokens":108}}}"#,
                "\n"
            ),
        )
        .expect("write transcript");

        let events =
            crate::sources::claude::parse_usage_file(&transcript).expect("scan transcript");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id.as_deref(), Some("ses-1"));
        assert_eq!(events[0].request_id.as_deref(), Some("req-1"));
        assert_eq!(events[0].dedupe_confidence, "exact");
        assert_eq!(events[0].tokens.total(), 52_864);
    }

    #[test]
    fn claude_file_parse_failures_preserve_successful_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let valid = tmp.path().join("valid.jsonl");
        let vanished = tmp.path().join("vanished.jsonl");
        std::fs::write(
            &valid,
            concat!(
                r#"{"type":"assistant","timestamp":1000,"message":{"id":"valid","usage":{"inputTokens":10}}}"#,
                "\n"
            ),
        )
        .expect("write valid transcript");
        std::fs::write(&vanished, "").expect("write disappearing transcript");
        let valid_metadata = usage_file_metadata(&valid).expect("valid metadata");
        let vanished_metadata = usage_file_metadata(&vanished).expect("vanished metadata");
        std::fs::remove_file(&vanished).expect("remove transcript");
        let missing = vec![
            (0, valid, valid_metadata),
            (1, vanished.clone(), vanished_metadata),
        ];
        let mut warnings = Vec::new();

        let parsed =
            parse_missing_usage_files("claude", &missing, &mut warnings, &|path: &Path| {
                crate::sources::claude::parse_usage_file(path).map(FileParse::cacheable)
            });

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].index, 0);
        assert_eq!(parsed[0].events.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(vanished.to_string_lossy().as_ref()));
    }

    #[test]
    fn permission_reviews_are_opt_in_for_cold_cached_memoized_usage_and_activity() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let sessions = tmp.path().join("sessions/2026/07/03");
        std::fs::create_dir_all(&sessions).expect("create sessions");
        for (id, origin) in [
            ("primary", serde_json::json!({"source": "cli"})),
            (
                "agent",
                serde_json::json!({"source": {"subagent": "worker"}}),
            ),
            (
                "review",
                serde_json::json!({"thread_source": "guardian_review"}),
            ),
            (
                "legacy-review",
                serde_json::json!({"source": {"subagent": {"other": "guardian"}}}),
            ),
        ] {
            let mut payload = origin;
            payload["id"] = id.into();
            payload["cwd"] = "/repo/memex".into();
            let metadata = serde_json::json!({"type": "session_meta", "payload": payload});
            let usage = serde_json::json!({
                "type": "event_msg", "timestamp": "2026-07-03T01:02:05Z",
                "payload": {"type": "token_count", "info": {
                    "last_token_usage": {"input_tokens": 100, "output_tokens": 25},
                    "total_token_usage": {"input_tokens": 100, "output_tokens": 25}
                }}
            });
            std::fs::write(
                sessions.join(format!("rollout-{id}.jsonl")),
                format!("{metadata}\n{usage}\n"),
            )
            .expect("write session");
        }
        let _env = EnvVarGuard::set_os(&[("CODEX_HOME", Some(tmp.path().as_os_str()))]);
        let mut query = UsageQuery {
            source: Some(SourceFilter::Codex),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };
        let cold = scan_usage(&query).expect("cold scan");
        assert_eq!(cold.events, 2);
        assert_eq!(cold.total_tokens, 250);
        assert!(cold.details.iter().all(|event| !event.permission_review));
        let warm = scan_usage(&query).expect("cached scan");
        assert_eq!(warm.events, 2);
        query.memo_ttl_ms = 60_000;
        assert_eq!(scan_usage(&query).unwrap().events, 2);
        query.include_reviews = true;
        assert_eq!(scan_usage(&query).unwrap().events, 4);
        assert_eq!(scan_usage_activity(&query).unwrap().0.len(), 4);
        query.include_reviews = false;
        assert_eq!(scan_usage_activity(&query).unwrap().0.len(), 2);
        query.memo_ttl_ms = 0;
        query.include_reviews = true;
        assert_eq!(scan_usage(&query).unwrap().total_tokens, 500);
    }

    #[test]
    fn codex_scanner_caches_events_by_file_metadata() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let sessions = tmp.path().join("sessions/2026/07/03");
        std::fs::create_dir_all(&sessions).expect("create sessions");
        std::fs::write(
            sessions.join("rollout-2026-07-03-session.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-03T01:02:03Z","payload":{"id":"codex-session","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-03T01:02:05Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":25},"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":25}}}}"#,
                "\n"
            ),
        )
        .expect("write session");
        let _env = EnvVarGuard::set_os(&[("CODEX_HOME", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Codex),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        let cold = scan_usage(&query).expect("cold scan");
        let warm = scan_usage(&query).expect("warm scan");
        let cache = Connection::open(query.cache_path.as_ref().expect("cache path"))
            .expect("open usage cache");
        let cached_files: u64 = cache
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'codex'",
                [],
                |row| row.get(0),
            )
            .expect("count cached files");

        assert_eq!(cold.events, 1);
        assert_eq!(cold.details[0].tokens.total(), 125);
        assert_eq!(cold.details[0].model.as_deref(), Some("gpt-5.4"));
        assert_eq!(cold.details[0].session_id.as_deref(), Some("codex-session"));
        assert_eq!(cold.details[0].project.as_deref(), Some("/repo/memex"));
        assert_eq!(warm.events, cold.events);
        assert_eq!(warm.total_tokens, cold.total_tokens);
        assert_eq!(cached_files, 1);
    }

    #[test]
    fn codex_fork_children_inherit_parent_baselines() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent_dir = tmp.path().join("sessions/2026/07/14");
        let child_dir = tmp.path().join("sessions/2026/07/15");
        std::fs::create_dir_all(&parent_dir).expect("create parent dir");
        std::fs::create_dir_all(&child_dir).expect("create child dir");
        std::fs::write(
            parent_dir.join("rollout-2026-07-14T10-00-00-019f0000-0000-7000-8000-000000000001.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:02:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200},"total_token_usage":{"input_tokens":300}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:03:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":300},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n"
            ),
        )
        .expect("write parent rollout");
        // The child replays a TRUNCATED parent history (the total=300 snapshot is missing)
        // under its own session id, so cross-file tuple dedupe cannot suppress it; only the
        // inherited parent baseline can.
        std::fs::write(
            child_dir.join("rollout-2026-07-15T09-00-00-019f0000-0000-7000-8000-000000000002.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-15T09:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000002","forked_from_id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:01Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:02Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":300},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:05:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":150},"total_token_usage":{"input_tokens":750}}}}"#,
                "\n"
            ),
        )
        .expect("write fork rollout");
        let _env = EnvVarGuard::set_os(&[("CODEX_HOME", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Codex),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        let report = scan_usage(&query).expect("scan usage");

        // Parent turns: 100 + 200 + 300. Child: only the post-fork turn of 150.
        assert_eq!(report.total_tokens, 750);
        assert_eq!(report.events, 4);
        let child_events: Vec<_> = report
            .details
            .iter()
            .filter(|event| event.source_path.contains("2026-07-15T09-00-00"))
            .collect();
        assert_eq!(child_events.len(), 1);
        assert_eq!(child_events[0].tokens.total(), 150);
        assert!(!child_events[0].conservative_undercount);
    }

    #[test]
    fn codex_unresolved_fork_is_not_cached_until_parent_appears() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let sessions = tmp.path().join("sessions");
        let parent_dir = sessions.join("2026/07/14");
        let child_dir = sessions.join("2026/07/15");
        std::fs::create_dir_all(&child_dir).expect("create child dir");
        let child = child_dir
            .join("rollout-2026-07-15T09-00-00-019f0000-0000-7000-8000-000000000002.jsonl");
        // Child replays the parent's total=100 and total=600 snapshots, then does one new
        // turn (total=750). With the parent absent the replay is counted via the guessed
        // baseline; with the parent present only the +150 turn should remain.
        std::fs::write(
            &child,
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-15T09:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000002","forked_from_id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:01Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:02Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:05:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":150},"total_token_usage":{"input_tokens":750}}}}"#,
                "\n"
            ),
        )
        .expect("write fork rollout");
        let _env = EnvVarGuard::set_os(&[("CODEX_HOME", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Codex),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        // Parent not yet on disk: fork is unresolved, so nothing is cached for it. Had the
        // guessed result been cached, the next scan would serve it and double-count the 500
        // replayed tokens on top of the parent's own count.
        scan_usage(&query).expect("scan without parent");
        let cache = Connection::open(query.cache_path.as_ref().expect("cache path"))
            .expect("open usage cache");
        let cached_files: u64 = cache
            .query_row(
                "SELECT count(*) FROM usage_file_cache WHERE source = 'codex'",
                [],
                |row| row.get(0),
            )
            .expect("count cached files");
        assert_eq!(cached_files, 0, "unresolved fork must not be cached");

        // Parent appears; the child file is byte-for-byte unchanged. Because the unresolved
        // result was never cached, this scan re-parses and resolves the baseline.
        std::fs::create_dir_all(&parent_dir).expect("create parent dir");
        std::fs::write(
            parent_dir
                .join("rollout-2026-07-14T10-00-00-019f0000-0000-7000-8000-000000000001.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:02:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n"
            ),
        )
        .expect("write parent rollout");

        let after = scan_usage(&query).expect("scan with parent");

        // Parent contributes 100 + 500; child only its new +150 turn.
        assert_eq!(after.total_tokens, 750);
        let child_after: u64 = after
            .details
            .iter()
            .filter(|event| {
                event
                    .source_path
                    .contains("019f0000-0000-7000-8000-000000000002")
            })
            .map(|event| event.tokens.total())
            .sum();
        assert_eq!(child_after, 150);
    }

    #[test]
    fn codex_nested_thread_spawn_parent_is_resolved() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent_dir = tmp.path().join("sessions/2026/07/14");
        let child_dir = tmp.path().join("sessions/2026/07/15");
        std::fs::create_dir_all(&parent_dir).expect("create parent dir");
        std::fs::create_dir_all(&child_dir).expect("create child dir");
        std::fs::write(
            parent_dir
                .join("rollout-2026-07-14T10-00-00-019f0000-0000-7000-8000-000000000001.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:02:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n"
            ),
        )
        .expect("write parent rollout");
        // The parent link is only present in the nested subagent thread_spawn shape.
        std::fs::write(
            child_dir
                .join("rollout-2026-07-15T09-00-00-019f0000-0000-7000-8000-000000000002.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-15T09:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000002","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019f0000-0000-7000-8000-000000000001"}}},"cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:01Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:02Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:05:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":150},"total_token_usage":{"input_tokens":750}}}}"#,
                "\n"
            ),
        )
        .expect("write nested fork rollout");
        let _env = EnvVarGuard::set_os(&[("CODEX_HOME", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Codex),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        let report = scan_usage(&query).expect("scan usage");

        // Parent 100 + 500; child replays both and adds only its 150 turn.
        assert_eq!(report.total_tokens, 750);
        let child: u64 = report
            .details
            .iter()
            .filter(|event| {
                event
                    .source_path
                    .contains("019f0000-0000-7000-8000-000000000002")
            })
            .map(|event| event.tokens.total())
            .sum();
        assert_eq!(child, 150);
    }

    #[test]
    fn codex_fork_merges_snapshots_from_duplicate_parent_copies() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        // The parent session exists in two roots: an archived copy truncated to the first
        // snapshot, and an active copy with the full pre-fork history. The child must inherit
        // the merged (fuller) baseline, not whichever copy is indexed first.
        let archived_dir = tmp.path().join("archived_sessions/2026/07/14");
        let active_dir = tmp.path().join("sessions/2026/07/14");
        let child_dir = tmp.path().join("sessions/2026/07/15");
        std::fs::create_dir_all(&archived_dir).expect("create archived dir");
        std::fs::create_dir_all(&active_dir).expect("create active dir");
        std::fs::create_dir_all(&child_dir).expect("create child dir");
        let parent_name = "rollout-2026-07-14T10-00-00-019f0000-0000-7000-8000-000000000001.jsonl";
        std::fs::write(
            archived_dir.join(parent_name),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n"
            ),
        )
        .expect("write archived parent copy");
        std::fs::write(
            active_dir.join(parent_name),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:02:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n"
            ),
        )
        .expect("write active parent copy");
        std::fs::write(
            child_dir
                .join("rollout-2026-07-15T09-00-00-019f0000-0000-7000-8000-000000000002.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-15T09:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000002","forked_from_id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:01Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:02Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:05:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":150},"total_token_usage":{"input_tokens":750}}}}"#,
                "\n"
            ),
        )
        .expect("write fork rollout");
        let _env = EnvVarGuard::set_os(&[("CODEX_HOME", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Codex),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        let report = scan_usage(&query).expect("scan usage");

        // The child replays both parent snapshots (100 and 600) and adds only its 150 turn.
        // Had it inherited from the truncated archived copy alone, the 500 would recount.
        let child: u64 = report
            .details
            .iter()
            .filter(|event| {
                event
                    .source_path
                    .contains("019f0000-0000-7000-8000-000000000002")
            })
            .map(|event| event.tokens.total())
            .sum();
        assert_eq!(child, 150);
    }

    #[test]
    fn codex_fork_reparses_when_a_new_parent_copy_appears() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let active_dir = tmp.path().join("sessions/2026/07/14");
        let archived_dir = tmp.path().join("archived_sessions/2026/07/14");
        let child_dir = tmp.path().join("sessions/2026/07/15");
        std::fs::create_dir_all(&active_dir).expect("create active dir");
        std::fs::create_dir_all(&child_dir).expect("create child dir");
        let parent_name = "rollout-2026-07-14T10-00-00-019f0000-0000-7000-8000-000000000001.jsonl";
        // At first only a truncated parent copy exists (just the total=100 snapshot).
        std::fs::write(
            active_dir.join(parent_name),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n"
            ),
        )
        .expect("write truncated parent copy");
        std::fs::write(
            child_dir
                .join("rollout-2026-07-15T09-00-00-019f0000-0000-7000-8000-000000000002.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-15T09:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000002","forked_from_id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:01Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:02Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:05:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":150},"total_token_usage":{"input_tokens":750}}}}"#,
                "\n"
            ),
        )
        .expect("write fork rollout");
        let _env = EnvVarGuard::set_os(&[("CODEX_HOME", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Codex),
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        // First scan: the only parent copy is truncated, so the child treats the not-yet-seen
        // total=600 snapshot as new. The child is cached with a dependency on that one copy.
        let before = scan_usage(&query).expect("first scan");
        assert_eq!(before.total_tokens, 750);

        // A fuller parent copy lands at a new (archived) path. The originally recorded copy is
        // untouched, so only the changed candidate set can trigger the child to re-parse.
        std::fs::create_dir_all(&archived_dir).expect("create archived dir");
        std::fs::write(
            archived_dir.join(parent_name),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:02:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n"
            ),
        )
        .expect("write fuller parent copy");

        let after = scan_usage(&query).expect("second scan");

        // Without candidate-set invalidation the child would stay cached and the fuller copy's
        // 500 would be counted twice (total 1250); re-parsing merges both copies and keeps 750.
        assert_eq!(after.total_tokens, 750);
    }

    #[test]
    fn codex_fork_reparses_when_partial_parent_is_extended() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent_dir = tmp.path().join("sessions/2026/07/14");
        let child_dir = tmp.path().join("sessions/2026/07/15");
        std::fs::create_dir_all(&parent_dir).expect("create parent dir");
        std::fs::create_dir_all(&child_dir).expect("create child dir");
        let parent = parent_dir
            .join("rollout-2026-07-14T10-00-00-019f0000-0000-7000-8000-000000000001.jsonl");
        // Parent is only partially synced: it has the total=100 snapshot but not yet the
        // total=600 snapshot the child replays.
        std::fs::write(
            &parent,
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n"
            ),
        )
        .expect("write partial parent");
        std::fs::write(
            child_dir
                .join("rollout-2026-07-15T09-00-00-019f0000-0000-7000-8000-000000000002.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-15T09:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000002","forked_from_id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:01Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:00:02Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-15T09:05:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":150},"total_token_usage":{"input_tokens":750}}}}"#,
                "\n"
            ),
        )
        .expect("write fork rollout");
        let _env = EnvVarGuard::set_os(&[("CODEX_HOME", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Codex),
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        // Partial parent: it emits only 100, and the child counts the not-yet-synced
        // total=600 snapshot as new (its baseline is the partial 100). The child result is
        // cached against the parent's current metadata.
        let partial = scan_usage(&query).expect("scan with partial parent");
        assert_eq!(partial.total_tokens, 750);

        // Parent finishes syncing the total=600 snapshot. The child file is unchanged, but
        // its cached dependency on the parent is now stale, so it must re-parse.
        std::fs::write(
            &parent,
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:02:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":500},"total_token_usage":{"input_tokens":600}}}}"#,
                "\n"
            ),
        )
        .expect("extend parent");

        let extended = scan_usage(&query).expect("scan with extended parent");

        // Without dependency invalidation the child would stay cached and the parent's newly
        // synced 500 would be counted twice (total 1250); re-parsing keeps it at 750.
        assert_eq!(extended.total_tokens, 750);
    }

    #[test]
    fn opencode_message_file_changes_bypass_volatile_reuse() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let message_dir = tmp.path().join("storage/message/ses_test");
        std::fs::create_dir_all(&message_dir).expect("create message directory");
        let message_path = message_dir.join("msg_test.json");
        let message = |output: u64| {
            serde_json::to_vec(&serde_json::json!({
                "id": "msg_test",
                "sessionID": "ses_test",
                "time": { "created": 1_750_000_000_000u64 },
                "tokens": {
                    "input": 10,
                    "output": output,
                    "reasoning": 0,
                    "cache": { "read": 0, "write": 0 }
                }
            }))
            .expect("serialize message")
        };
        std::fs::write(&message_path, message(5)).expect("write message");
        let _env = EnvVarGuard::set_os(&[("OPENCODE_DATA_DIR", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Opencode),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            ..UsageQuery::default()
        };

        let initial = scan_usage(&query).expect("initial scan");
        // The message file is rewritten while a response streams; unlike the opencode
        // databases it must not be served from the 60s volatile window.
        std::fs::write(&message_path, message(500)).expect("rewrite message");
        let updated = scan_usage(&query).expect("updated scan");

        assert_eq!(initial.total_tokens, 15);
        assert_eq!(updated.total_tokens, 510);
    }

    #[test]
    fn memoized_scan_reuses_assembled_events_within_ttl() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        let transcript = projects.join("session.jsonl");
        let line = |input: u64| {
            format!(
                r#"{{"type":"assistant","sessionId":"session","timestamp":1000,"message":{{"id":"m-{input}","usage":{{"inputTokens":{input}}}}}}}"#
            ) + "\n"
        };
        std::fs::write(&transcript, line(10)).expect("write transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            memo_ttl_ms: 60_000,
            ..UsageQuery::default()
        };

        let first = scan_usage(&query).expect("first scan");
        std::fs::write(&transcript, format!("{}{}", line(10), line(70))).expect("grow transcript");
        let memoized = scan_usage(&query).expect("memoized scan");
        let fresh = scan_usage(&UsageQuery {
            memo_ttl_ms: 0,
            ..query.clone()
        })
        .expect("fresh scan");

        assert_eq!(first.total_tokens, 10);
        assert_eq!(memoized.total_tokens, 10);
        assert_eq!(fresh.total_tokens, 80);
        // An uncached query must not evict another caller's still-valid memo.
        assert_eq!(scan_usage(&query).unwrap().total_tokens, 10);
    }

    #[test]
    fn alternating_source_filters_reuse_snapshots() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        std::fs::write(
            projects.join("session.jsonl"),
            r#"{"type":"assistant","sessionId":"s","timestamp":1000,"cwd":"/repo/memex","message":{"id":"m","model":"claude-sonnet-4-6","usage":{"inputTokens":10}}}"#
                .to_string()
                + "\n",
        )
        .expect("write transcript");
        let sessions = tmp.path().join("sessions/2026/07/14");
        std::fs::create_dir_all(&sessions).expect("create sessions");
        std::fs::write(
            sessions.join(
                "rollout-2026-07-14T10-00-00-019f0000-0000-7000-8000-000000000001.jsonl",
            ),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-07-14T10:01:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100},"total_token_usage":{"input_tokens":100}}}}"#,
                "\n",
            ),
        )
        .expect("write rollout");
        let _env = EnvVarGuard::set_os(&[
            ("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str())),
            ("CODEX_HOME", Some(tmp.path().as_os_str())),
        ]);
        let cache = tmp.path().join("usage-cache.sqlite3");
        let base = UsageQuery {
            cache_path: Some(cache.clone()),
            memo_ttl_ms: 60_000,
            ..UsageQuery::default()
        };
        let codex = UsageQuery {
            source: Some(SourceFilter::Codex),
            ..base.clone()
        };
        let claude = UsageQuery {
            source: Some(SourceFilter::Claude),
            ..base.clone()
        };

        assert_eq!(scan_usage(&codex).expect("codex").events, 1);
        assert_eq!(scan_usage(&claude).expect("claude").events, 1);
        // Alternating back reuses both snapshots; a single slot would have evicted
        // codex when claude was queried and would rescan here.
        assert_eq!(scan_usage(&codex).expect("codex again").events, 1);
        assert_eq!(scan_usage(&claude).expect("claude again").events, 1);
        let store = lock_partitions();
        assert!(
            store.contains_key(&(SourceFilter::Codex, Some(cache.clone()))),
            "codex snapshot retained"
        );
        assert!(
            store.contains_key(&(SourceFilter::Claude, Some(cache))),
            "claude snapshot retained"
        );
    }

    #[test]
    fn stale_snapshot_revalidates_without_rebuild_when_unchanged() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        let transcript = projects.join("session.jsonl");
        let line = |input: u64| {
            format!(
                r#"{{"type":"assistant","sessionId":"session","timestamp":1000,"message":{{"id":"m-{input}","usage":{{"inputTokens":{input}}}}}}}"#
            ) + "\n"
        };
        std::fs::write(&transcript, line(10)).expect("write transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let cache = tmp.path().join("usage-cache.sqlite3");
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            cache_path: Some(cache.clone()),
            memo_ttl_ms: 1,
            ..UsageQuery::default()
        };
        let key = (SourceFilter::Claude, Some(cache));

        assert_eq!(scan_usage(&query).expect("cold scan").total_tokens, 10);
        let fingerprint = lock_partitions()
            .get(&key)
            .map(|entry| entry.fingerprint.clone())
            .expect("snapshot published");
        assert!(
            check_partition_valid(key.0, key.1.as_deref(), &fingerprint),
            "unchanged corpus revalidates"
        );
        std::fs::write(&transcript, format!("{}{}", line(10), line(70))).expect("grow transcript");
        assert!(
            !check_partition_valid(key.0, key.1.as_deref(), &fingerprint),
            "appended transcript invalidates"
        );
        // Let the 1ms TTL lapse so the next query revalidates instead of reusing.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(scan_usage(&query).expect("refresh").total_tokens, 80);
        let fingerprint = lock_partitions()
            .get(&key)
            .map(|entry| entry.fingerprint.clone())
            .expect("snapshot republished");
        assert!(
            check_partition_valid(key.0, key.1.as_deref(), &fingerprint),
            "refreshed corpus revalidates"
        );
    }

    #[test]
    fn combined_query_merges_partitions_in_global_order() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).expect("create empty dir");
        let projects = tmp.path().join("claude/projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        let line = |id: &str, timestamp_ms: u64, input: u64| {
            format!(
                r#"{{"type":"assistant","sessionId":"session","timestamp":{timestamp_ms},"cwd":"/repo/memex","message":{{"id":"{id}","model":"claude-sonnet-4-6","usage":{{"inputTokens":{input}}}}}}}"#
            ) + "\n"
        };
        // Timestamps interleave with the codex event below: 1M, 3M vs 2M.
        std::fs::write(&projects.join("session.jsonl"), line("m-10", 1000, 10))
            .expect("write transcript");
        std::fs::write(&projects.join("later.jsonl"), line("m-70", 3000, 70))
            .expect("write later transcript");
        let sessions = tmp.path().join("codex/sessions/2026/07/14");
        std::fs::create_dir_all(&sessions).expect("create sessions");
        std::fs::write(
            sessions.join(
                "rollout-2026-07-14T10-00-00-019f0000-0000-7000-8000-000000000001.jsonl",
            ),
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-07-14T10:00:00Z","payload":{"id":"019f0000-0000-7000-8000-000000000001","cwd":"/repo/memex"}}"#,
                "\n",
                // Numeric 2000 means epoch seconds here: 2_000_000 ms, between the two.
                r#"{"type":"event_msg","timestamp":2000,"payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":12345},"total_token_usage":{"input_tokens":12345}}}}"#,
                "\n",
            ),
        )
        .expect("write rollout");
        let _env = EnvVarGuard::set_os(&[
            (
                "CLAUDE_CONFIG_DIR",
                Some(tmp.path().join("claude").as_os_str()),
            ),
            ("CODEX_HOME", Some(tmp.path().join("codex").as_os_str())),
            ("OPENCODE_DATA_DIR", Some(empty.as_os_str())),
            ("COPILOT_HOME", Some(empty.as_os_str())),
            ("PI_CODING_AGENT_DIR", Some(empty.as_os_str())),
            ("OPENCLAW_STATE_DIR", Some(empty.as_os_str())),
            ("GROK_HOME", Some(empty.as_os_str())),
            ("HERMES_PROFILE_ROOTS", Some(empty.as_os_str())),
            ("JCODE_HOME", Some(empty.as_os_str())),
            ("MUSE_HOME", Some(empty.as_os_str())),
            ("ANTIGRAVITY_HOME", Some(empty.as_os_str())),
        ]);
        let cache = tmp.path().join("usage-cache.sqlite3");
        let base = UsageQuery {
            include_events: true,
            cache_path: Some(cache),
            memo_ttl_ms: 60_000,
            ..UsageQuery::default()
        };
        let claude = UsageQuery {
            source: Some(SourceFilter::Claude),
            ..base.clone()
        };
        let codex = UsageQuery {
            source: Some(SourceFilter::Codex),
            ..base.clone()
        };
        let combined = UsageQuery {
            source: None,
            ..base.clone()
        };

        let claude_report = scan_usage(&claude).expect("claude report");
        let codex_report = scan_usage(&codex).expect("codex report");
        assert_eq!(claude_report.events, 2);
        assert_eq!(claude_report.total_tokens, 80);
        assert_eq!(codex_report.events, 1);
        assert_eq!(codex_report.total_tokens, 12345);

        let report = scan_usage(&combined).expect("combined report");
        // Per-source rows through the merged path match the single-source reports
        // built from the same partitions.
        for single in [&claude_report, &codex_report] {
            let source = single.by_source[0].source.clone();
            let merged_row = report
                .by_source
                .iter()
                .find(|row| row.source == source)
                .expect("source row in combined report");
            assert_eq!(
                serde_json::to_value(merged_row).unwrap(),
                serde_json::to_value(&single.by_source[0]).unwrap(),
                "merged aggregation matches single-source aggregation for {source}"
            );
        }
        // Our three events interleave across partitions in global timestamp order.
        // (Other sources may contribute their own events on a dev machine; the
        // cursor home cannot be overridden, so only assert on our sessions.)
        let ours: Vec<u64> = report
            .details
            .iter()
            .filter(|event| {
                event.session_id.as_deref() == Some("session")
                    || event.session_id.as_deref() == Some("019f0000-0000-7000-8000-000000000001")
            })
            .map(|event| event.timestamp_ms)
            .collect();
        assert_eq!(ours, vec![1_000_000, 2_000_000, 3_000_000]);
        // The whole detail stream (whatever it contains) is globally ordered.
        let mut ordered = report.details.clone();
        ordered.sort_by(|a, b| {
            (a.timestamp_ms, &a.source_path, a.source_order).cmp(&(
                b.timestamp_ms,
                &b.source_path,
                b.source_order,
            ))
        });
        assert_eq!(
            serde_json::to_value(&report.details).unwrap(),
            serde_json::to_value(&ordered).unwrap(),
            "combined details are globally ordered"
        );
        // Activity points visit in the same merged order.
        let (points, _) = scan_usage_activity(&combined).expect("combined activity");
        let mut point_order: Vec<u64> = points.iter().map(|point| point.timestamp_ms).collect();
        let sorted = {
            let mut sorted = point_order.clone();
            sorted.sort_unstable();
            sorted
        };
        assert_eq!(point_order, sorted, "activity points are globally ordered");
    }

    #[test]
    fn facts_report_matches_assembly_report() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        let line = |id: &str, timestamp_ms: u64, input: u64| {
            format!(
                r#"{{"type":"assistant","sessionId":"session","timestamp":{timestamp_ms},"cwd":"/repo/memex","message":{{"id":"{id}","model":"claude-sonnet-4-6","usage":{{"inputTokens":{input}}}}}}}"#
            ) + "\n"
        };
        std::fs::write(&projects.join("session.jsonl"), line("m-10", 1000, 10))
            .expect("write transcript");
        std::fs::write(&projects.join("later.jsonl"), line("m-70", 3000, 70))
            .expect("write later transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let cache = tmp.path().join("usage-cache.sqlite3");
        let base = UsageQuery {
            source: Some(SourceFilter::Claude),
            include_events: true,
            cache_path: Some(cache.clone()),
            memo_ttl_ms: 60_000,
            ..UsageQuery::default()
        };

        // Populate facts through a normal scan first.
        let assembly_report = scan_usage(&base).expect("assembly report");
        assert_eq!(assembly_report.events, 2);
        let warnings = assembly_report.warnings.clone();
        let facts_report = scan_usage_from_facts(&base, &cache, &warnings).expect("facts report");
        assert_eq!(
            serde_json::to_value(&facts_report).unwrap(),
            serde_json::to_value(&assembly_report).unwrap(),
            "facts report is byte-identical to the assembly report"
        );
        // Same across cost modes, time bounds, and session filters.
        for query in [
            UsageQuery {
                cost_mode: CostMode::Source,
                ..base.clone()
            },
            UsageQuery {
                cost_mode: CostMode::Reprice,
                ..base.clone()
            },
            UsageQuery {
                since_ms: Some(2_000_000),
                ..base.clone()
            },
            UsageQuery {
                session_keys: Some(
                    [("claude".to_string(), "session".to_string())]
                        .into_iter()
                        .collect(),
                ),
                ..base.clone()
            },
        ] {
            let assembly_report = scan_usage(&query).expect("assembly report");
            let facts_report = scan_usage_from_facts(&query, &cache, &assembly_report.warnings)
                .expect("facts report");
            assert_eq!(
                serde_json::to_value(&facts_report).unwrap(),
                serde_json::to_value(&assembly_report).unwrap(),
                "facts report matches for {query:?}"
            );
        }
        // Activity points match exactly too.
        let mut assembly_points = Vec::new();
        visit_inner(&base, &mut |point| assembly_points.push(point)).expect("visit");
        let facts_points = read_fact_points(&base, &cache).expect("fact points");
        assert_eq!(facts_points, assembly_points);
    }

    #[test]
    fn facts_refresh_matches_legacy_rebuild_after_append() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        let line = |id: &str, timestamp_ms: u64, input: u64| {
            format!(
                r#"{{"type":"assistant","sessionId":"session","timestamp":{timestamp_ms},"cwd":"/repo/memex","message":{{"id":"{id}","model":"claude-sonnet-4-6","usage":{{"inputTokens":{input}}}}}}}"#
            ) + "\n"
        };
        std::fs::write(&projects.join("a.jsonl"), line("m-10", 1000, 10))
            .expect("write transcript");
        std::fs::write(&projects.join("b.jsonl"), line("m-20", 2000, 20))
            .expect("write transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            include_events: true,
            cache_path: Some(tmp.path().join("usage-cache.sqlite3")),
            memo_ttl_ms: 1,
            ..UsageQuery::default()
        };

        // Legacy path populates blobs and facts on first build (no sync row yet).
        let cold = scan_usage(&query).expect("cold scan");
        assert_eq!(cold.total_tokens, 30);
        std::thread::sleep(std::time::Duration::from_millis(5));
        // Second scan finds a sync row and valid blobs: still legacy (facts path
        // needs no work), then an append forces a facts-backed refresh.
        std::fs::write(
            &projects.join("a.jsonl"),
            format!("{}{}", line("m-10", 1000, 10), line("m-30", 3000, 30)),
        )
        .expect("grow transcript");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let refreshed = scan_usage(&query).expect("facts refresh");
        assert_eq!(refreshed.total_tokens, 60);
        // A from-scratch legacy rebuild over the same corpus must agree exactly.
        let legacy = scan_usage(&UsageQuery {
            cache_path: Some(tmp.path().join("fresh-cache.sqlite3")),
            ..query.clone()
        })
        .expect("legacy rebuild");
        assert_eq!(
            serde_json::to_value(&refreshed).unwrap(),
            serde_json::to_value(&legacy).unwrap(),
            "facts-backed refresh matches a legacy rebuild"
        );
    }

    #[test]
    fn missing_sync_row_migrates_through_legacy_rebuild() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        std::fs::write(
            &projects.join("session.jsonl"),
            r#"{"type":"assistant","sessionId":"session","timestamp":1000,"cwd":"/repo/memex","message":{"id":"m","model":"claude-sonnet-4-6","usage":{"inputTokens":10}}}"#
                .to_string()
                + "\n",
        )
        .expect("write transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let cache = tmp.path().join("usage-cache.sqlite3");
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            cache_path: Some(cache.clone()),
            memo_ttl_ms: 1,
            ..UsageQuery::default()
        };
        assert_eq!(scan_usage(&query).expect("cold scan").total_tokens, 10);
        // Simulate a pre-facts database: blob rows exist, sync row does not.
        Connection::open(&cache)
            .expect("open cache")
            .execute("DELETE FROM usage_fact_sync", [])
            .expect("drop sync row");
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(scan_usage(&query).expect("migrating scan").total_tokens, 10);
        // And the sync row is back for the next check.
        let sync_rows: u64 = Connection::open(&cache)
            .expect("reopen cache")
            .query_row(
                "SELECT count(*) FROM usage_fact_sync WHERE source = 'claude'",
                [],
                |row| row.get(0),
            )
            .expect("count sync rows");
        assert_eq!(sync_rows, 1);
    }

    #[test]
    fn cold_serve_matches_normal_report_and_repopulates() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        lock_partitions().clear();
        lock_merged().clear();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        std::fs::write(
            &projects.join("session.jsonl"),
            r#"{"type":"assistant","sessionId":"session","timestamp":1000,"cwd":"/repo/memex","message":{"id":"m","model":"claude-sonnet-4-6","usage":{"inputTokens":10}}}"#
                .to_string()
                + "\n",
        )
        .expect("write transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let cache = tmp.path().join("usage-cache.sqlite3");
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            include_events: true,
            cache_path: Some(cache.clone()),
            memo_ttl_ms: 60_000,
            ..UsageQuery::default()
        };
        let key = (SourceFilter::Claude, Some(cache));

        let first = scan_usage(&query).expect("first scan");
        assert!(lock_partitions().contains_key(&key));
        // Simulate a fresh process: drop all retained state, keep the database.
        lock_partitions().clear();
        lock_merged().clear();
        let cold = scan_usage(&query).expect("cold scan");
        assert_eq!(
            serde_json::to_value(&cold).unwrap(),
            serde_json::to_value(&first).unwrap(),
            "cold facts serve matches the normal report"
        );
        assert!(
            lock_partitions().contains_key(&key),
            "cold serve repopulates for retaining queries"
        );
    }

    #[test]
    fn cold_serve_oneshot_retains_nothing() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        lock_partitions().clear();
        lock_merged().clear();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        std::fs::write(
            &projects.join("session.jsonl"),
            r#"{"type":"assistant","sessionId":"session","timestamp":1000,"cwd":"/repo/memex","message":{"id":"m","model":"claude-sonnet-4-6","usage":{"inputTokens":10}}}"#
                .to_string()
                + "\n",
        )
        .expect("write transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let cache = tmp.path().join("usage-cache.sqlite3");
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            cache_path: Some(cache.clone()),
            ..UsageQuery::default()
        };
        let key = (SourceFilter::Claude, Some(cache));

        // Warm the database through a retaining query, then forget everything.
        let retaining = UsageQuery {
            memo_ttl_ms: 60_000,
            ..query.clone()
        };
        let expected = scan_usage(&retaining).expect("retaining scan");
        lock_partitions().clear();
        lock_merged().clear();
        let cold = scan_usage(&query).expect("one-shot cold scan");
        assert_eq!(cold.total_tokens, expected.total_tokens);
        assert!(
            !lock_partitions().contains_key(&key),
            "one-shot cold serve retains nothing"
        );
    }

    #[test]
    fn cold_serve_returns_stored_warnings() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        lock_partitions().clear();
        lock_merged().clear();
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects/memex");
        std::fs::create_dir_all(&projects).expect("create projects");
        std::fs::write(
            &projects.join("session.jsonl"),
            r#"{"type":"assistant","sessionId":"session","timestamp":1000,"cwd":"/repo/memex","message":{"id":"m","model":"claude-sonnet-4-6","usage":{"inputTokens":10}}}"#
                .to_string()
                + "\n",
        )
        .expect("write transcript");
        let _env = EnvVarGuard::set_os(&[("CLAUDE_CONFIG_DIR", Some(tmp.path().as_os_str()))]);
        let cache = tmp.path().join("usage-cache.sqlite3");
        let query = UsageQuery {
            source: Some(SourceFilter::Claude),
            cache_path: Some(cache.clone()),
            memo_ttl_ms: 60_000,
            ..UsageQuery::default()
        };
        let key = (SourceFilter::Claude, Some(cache.clone()));

        assert_eq!(scan_usage(&query).expect("scan").total_tokens, 10);
        // Stamp stored warnings directly, then forget all retained state.
        let fingerprint = lock_partitions()
            .get(&key)
            .map(|entry| entry.fingerprint.clone())
            .expect("snapshot published");
        let stored = Connection::open(&cache).expect("open cache");
        write_fact_sync(
            &stored,
            SourceFilter::Claude,
            &fingerprint,
            &["nightly lint".to_string()],
        )
        .expect("store warnings");
        drop(stored);
        lock_partitions().clear();
        lock_merged().clear();
        let cold = scan_usage(&query).expect("cold scan");
        assert_eq!(cold.total_tokens, 10);
        assert_eq!(cold.warnings, vec!["nightly lint".to_string()]);
    }

    #[test]
    fn opencode_project_filter_matches_indexed_project() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let message_dir = tmp.path().join("storage/message/ses_test");
        std::fs::create_dir_all(&message_dir).expect("create message directory");
        std::fs::write(
            message_dir.join("msg_test.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "msg_test",
                "sessionID": "ses_test",
                "path": { "cwd": "/repo/memex" },
                "time": { "created": 1_750_000_000_000u64 },
                "tokens": {
                    "input": 10,
                    "output": 5,
                    "reasoning": 0,
                    "cache": { "read": 0, "write": 0 }
                }
            }))
            .expect("serialize message"),
        )
        .expect("write message");
        let _env = EnvVarGuard::set_os(&[("OPENCODE_DATA_DIR", Some(tmp.path().as_os_str()))]);
        let mut query = UsageQuery {
            source: Some(SourceFilter::Opencode),
            project: Some("opencode".into()),
            project_grouping: ProjectGrouping::Flat,
            include_events: true,
            ..UsageQuery::default()
        };

        let matching = scan_usage(&query).expect("scan matching project");
        query.project = Some("memex".into());
        let mismatched = scan_usage(&query).expect("scan mismatched project");
        query.project = Some("opencode".into());
        query.session_keys = Some(HashSet::from([("opencode".into(), "ses_test".into())]));
        let matching_session = scan_usage(&query).expect("scan matching session");
        query.session_keys = Some(HashSet::from([("opencode".into(), "ses_other".into())]));
        let mismatched_session = scan_usage(&query).expect("scan mismatched session");

        assert_eq!(matching.events, 1);
        assert_eq!(matching.details[0].project.as_deref(), Some("opencode"));
        assert_eq!(mismatched.events, 0);
        assert_eq!(matching_session.events, 1);
        assert_eq!(mismatched_session.events, 0);
    }

    #[test]
    fn cursor_project_mapping_is_recomputed_on_cache_hits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("state.vscdb");
        let conn = Connection::open(&db_path).expect("create cursor db");
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT); \
             INSERT INTO cursorDiskKV VALUES ('composerData:composer-main', \
             '{\"generationUUID\":\"gen-1\",\"inputTokens\":10,\"outputTokens\":5}');",
        )
        .expect("populate cursor db");
        drop(conn);
        let cache_path = tmp.path().join("usage-cache.sqlite3");
        let files = vec![db_path];
        let run = |project_by_session: &HashMap<String, String>| {
            let mut cache = UsageCache::open(&cache_path).expect("open cache");
            let mut warnings = Vec::new();
            let mut events = Vec::new();
            scan_files_cached(
                SourceScan {
                    source: "cursor",
                    parser_version: crate::sources::cursor::VERSIONS.usage,
                    volatile_reuse_ms: |_| Some(VOLATILE_DB_REUSE_MS),
                },
                &files,
                Some(&mut cache),
                &mut warnings,
                &mut events,
                |path| crate::sources::cursor::parse_usage_database(path).map(FileParse::cacheable),
            );
            assert_eq!(warnings, Vec::<String>::new());
            crate::sources::cursor::apply_projects(&mut events, project_by_session);
            events
        };

        // Cold scan before any transcript is indexed: no attribution.
        let cold = run(&HashMap::new());
        // The database is unchanged, so this scan is served from the cache; a transcript
        // mapping discovered afterwards must still take effect.
        let warm = run(&HashMap::from([(
            "composer-main".to_string(),
            "memex".to_string(),
        )]));

        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].project, None);
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0].project.as_deref(), Some("memex"));
    }

    #[test]
    fn usage_project_matching_normalizes_paths_slugs_and_remotes() {
        let mut cache = HashMap::new();

        for candidate in [
            "/Users/nico/Code/memex",
            "--Users-nico-Code-memex--",
            "git@github.com:nicosuave/memex.git",
        ] {
            assert!(usage_project_matches(
                candidate,
                "memex",
                ProjectGrouping::Flat,
                &mut cache,
            ));
        }
        assert!(!usage_project_matches(
            "/Users/nico/Code/other",
            "memex",
            ProjectGrouping::Flat,
            &mut cache,
        ));
    }

    #[test]
    fn repository_usage_groups_absolute_non_git_paths_as_unfiled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let standalone = tmp.path().join("generated-task-name");
        fs::create_dir(&standalone).expect("standalone dir");
        let mut cache = HashMap::new();

        assert!(usage_project_matches(
            standalone.to_string_lossy().as_ref(),
            crate::analytics::UNFILED_PROJECT,
            ProjectGrouping::Repository,
            &mut cache,
        ));
        assert!(usage_project_matches(
            "/missing/home/.codex/worktrees/8952/memex",
            "memex",
            ProjectGrouping::Repository,
            &mut cache,
        ));
        assert!(usage_project_matches(
            "memex",
            "memex",
            ProjectGrouping::Repository,
            &mut cache,
        ));
    }

    #[test]
    fn pi_scanner_uses_configured_session_directory() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_root = tmp.path().join("pi-agent");
        let omp_root = tmp.path().join("omp");
        let session_root = agent_root.join("custom/sessions/--C--Users-alice-Code-memex--");
        std::fs::create_dir_all(&session_root).expect("create session root");
        std::fs::write(
            agent_root.join("settings.json"),
            r#"{ "sessionDir": "custom/sessions" }"#,
        )
        .expect("write settings");
        std::fs::write(
            session_root.join("session.jsonl"),
            concat!(
                r#"{"type":"message","id":"a1","timestamp":"2026-07-03T01:02:05Z","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-4-6","usage":{"input":10,"cacheRead":2,"cacheWrite":3,"output":4}}}"#,
                "\n"
            ),
        )
        .expect("write session");
        let _env = EnvVarGuard::set_os(&[
            ("PI_CODING_AGENT_SESSION_DIR", None),
            ("PI_CODING_AGENT_DIR", Some(agent_root.as_os_str())),
            ("PI_CONFIG_DIR", Some(omp_root.as_os_str())),
            ("XDG_DATA_HOME", None),
        ]);
        let report = scan_usage(&UsageQuery {
            source: Some(SourceFilter::Pi),
            project: Some("memex".into()),
            project_grouping: ProjectGrouping::Flat,
            include_events: true,
            ..UsageQuery::default()
        })
        .expect("scan pi");

        assert!(report.warnings.is_empty());
        assert_eq!(report.events, 1);
        assert_eq!(report.details[0].tokens.total(), 19);
        assert_eq!(report.details[0].project.as_deref(), Some("memex"));
        assert!(
            report.details[0]
                .source_path
                .ends_with("custom/sessions/--C--Users-alice-Code-memex--/session.jsonl")
        );
    }

    #[test]
    fn pi_scanner_matches_indexed_header_and_filename_session_ids() {
        use crate::test_support::{EnvVarGuard, env_lock};

        let _guard = env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let omp_root = tmp.path().join("omp");
        let session_root = tmp.path().join("--Users-nico-Code-other--");
        std::fs::create_dir_all(&session_root).expect("create session root");

        let filename_id = "11111111-1111-1111-1111-111111111111";
        let header_id = "22222222-2222-2222-2222-222222222222";
        std::fs::write(
            session_root.join(format!("20260703T010203Z_{filename_id}.jsonl")),
            format!(
                concat!(
                    r#"{{"type":"session","id":"{header_id}","cwd":"/Users/nico/Code/memex"}}"#,
                    "\n",
                    r#"{{"type":"message","id":"a1","timestamp":"2026-07-03T01:02:05Z","message":{{"role":"assistant","usage":{{"input":10,"output":4}}}}}}"#,
                    "\n"
                ),
                header_id = header_id,
            ),
        )
        .expect("write header session");

        let fallback_id = "33333333-3333-3333-3333-333333333333";
        let fallback_stem = format!("20260703T010204Z_{fallback_id}");
        std::fs::write(
            session_root.join(format!("{fallback_stem}.jsonl")),
            concat!(
                r#"{"type":"message","id":"a2","timestamp":"2026-07-03T01:02:06Z","message":{"role":"assistant","usage":{"input":20,"output":5}}}"#,
                "\n"
            ),
        )
        .expect("write filename session");

        let _env = EnvVarGuard::set_os(&[
            ("PI_CODING_AGENT_SESSION_DIR", Some(tmp.path().as_os_str())),
            ("PI_CODING_AGENT_DIR", None),
            ("PI_CONFIG_DIR", Some(omp_root.as_os_str())),
            ("XDG_DATA_HOME", None),
        ]);
        let mut query = UsageQuery {
            source: Some(SourceFilter::Pi),
            include_events: true,
            ..UsageQuery::default()
        };

        query.session_keys = Some(HashSet::from([("pi".into(), header_id.into())]));
        let header = scan_usage(&query).expect("scan header session");
        query.session_keys = Some(HashSet::from([("pi".into(), filename_id.into())]));
        let overridden_filename = scan_usage(&query).expect("scan overridden filename session");
        query.session_keys = Some(HashSet::from([("pi".into(), fallback_id.into())]));
        let fallback = scan_usage(&query).expect("scan filename session");
        query.session_keys = Some(HashSet::from([("pi".into(), fallback_stem)]));
        let full_stem = scan_usage(&query).expect("scan full filename stem");

        assert_eq!(header.events, 1);
        assert_eq!(header.details[0].session_id.as_deref(), Some(header_id));
        assert_eq!(header.details[0].project.as_deref(), Some("memex"));
        assert_eq!(overridden_filename.events, 0);
        assert_eq!(fallback.events, 1);
        assert_eq!(fallback.details[0].session_id.as_deref(), Some(fallback_id));
        assert_eq!(full_stem.events, 0);
    }

    #[test]
    fn claude_cache_write_durations_get_distinct_rates() {
        let mut tokens = TokenBuckets::disjoint(100, 40, 30, 20);
        tokens.cache_write_1h = 10;
        let event = UsageEvent {
            source: "claude",
            source_path: "x".into(),
            source_record_id: None,
            session_id: None,
            request_id: None,
            message_id: None,
            timestamp_ms: 0,
            project: None,
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-6".into()),
            tokens,
            source_cost_usd: None,
            cost_authoritative: false,
            dedupe_confidence: "exact",
            conservative_undercount: false,
            cache_chain_excluded: false,
            sidechain: false,
            permission_review: false,
            source_order: 0,
        };
        // 100*3 + 40*.3 + 20*3.75 + 10*6 + 20*15 = $0.000747
        assert_eq!(calculated_cost_nanos(&event), Some(747_000));
    }

    #[test]
    fn auto_cost_honors_explicit_zero_source_cost() {
        let event = UsageEvent {
            source: "claude",
            source_path: "x".into(),
            source_record_id: None,
            session_id: None,
            request_id: None,
            message_id: None,
            timestamp_ms: 0,
            project: None,
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4-6".into()),
            tokens: TokenBuckets::disjoint(100, 0, 0, 0),
            source_cost_usd: Some(0.0),
            cost_authoritative: false,
            dedupe_confidence: "exact",
            conservative_undercount: false,
            cache_chain_excluded: false,
            sidechain: false,
            permission_review: false,
            source_order: 0,
        };
        assert_eq!(event_cost_nanos(&event, CostMode::Auto), Some(0));
        assert_eq!(event_cost_nanos(&event, CostMode::Reprice), Some(300_000));
    }

    #[test]
    fn implementation_only_event_state_is_absent_from_public_json_and_cached_internally() {
        let mut event = cache_event("session", 0, "claude-sonnet-4-6", 100, 0, 0);
        event.cost_authoritative = true;
        event.cache_chain_excluded = true;
        event.sidechain = true;
        event.permission_review = true;
        event.source_order = 42;

        let json = serde_json::to_value(&event).unwrap();
        let object = json.as_object().unwrap();
        assert!(object.contains_key("source"));
        assert!(object.contains_key("model"));
        assert!(!object.contains_key("cost_authoritative"));
        assert!(!object.contains_key("cache_chain_excluded"));
        assert!(!object.contains_key("sidechain"));
        assert!(!object.contains_key("permission_review"));
        assert!(!object.contains_key("source_order"));

        let bytes = postcard::to_stdvec(&CachedUsageEvent::from_event(&event)).unwrap();
        let cached: CachedUsageEvent = postcard::from_bytes(&bytes).unwrap();
        assert!(cached.cost_authoritative);
        assert!(cached.cache_chain_excluded);
        let restored = cached.into_event("claude", Arc::from("cached"));
        assert!(restored.cost_authoritative);
        assert!(restored.cache_chain_excluded);
        assert!(restored.permission_review);
    }
}
