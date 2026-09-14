# Usage cache benchmark

Measured September 13, 2026 on **nicbook-atm** (Apple M5 Pro, 64 GiB RAM,
macOS 26.6.2, Rust 1.95.0), against baseline commit `b199e6f`.

The workload uses a frozen copy of real **Codex-only** logs: 3,879 files, 7.1 GiB,
211,577 assembled events, and 205,577 events after the default review filter.
Both versions read the same copied files and isolated SQLite cache; neither the
running daemon nor the original logs were modified. These are measurements of
an **isolated benchmark process**, not the running daemon or the larger local corpus.
Final runs used the stock allocator, without allocator environment overrides or
pressure-relief calls.

## Results

Timing values are medians of three alternating baseline/candidate runs in fresh
processes with a warm SQLite usage cache. The app-style chart aggregates daily UTC
token buckets: baseline first collects activity points, while the candidate uses
the streaming visitor used by Home activity aggregation.

| Operation | Baseline | Candidate | Change |
| --- | ---: | ---: | ---: |
| First app-style chart, empty in-process memo | 146.8 ms | 193.9 ms | +47.0 ms |
| Warm app-style chart, per query over 20 calls | 4.33 ms | 2.31 ms | 47% faster |
| Expired-memo refresh and app-style chart | 158.7 ms | 183.1 ms | +24.4 ms |
| Warm summary report, no detailed events | 36.6 ms | 39.4 ms | +2.8 ms |
| One-shot chart with memo disabled | 167.0 ms | 164.5 ms | Essentially unchanged |
| Warm legacy API returning a point vector | 2.82 ms | 1.96 ms | 31% faster |

Compaction has a measurable initial-build and refresh cost. Warm charts improve;
this is not a claim that every query becomes faster. First/refresh timings also
include result hashing; warm chart timings exclude full-point hashing.

| Memory measurement | Baseline | Candidate | Reduction |
| --- | ---: | ---: | ---: |
| Retained Rust allocations after 20 streamed refreshes | 111.8 MiB | 40.7 MiB | 64% |
| Peak Rust allocations during a refresh, median | 286.1 MiB | 198.5 MiB | 31% |
| Whole-process physical footprint after 20 streamed refreshes | 415.2 MiB | 184.3 MiB | 56% |

The sustained-run footprint at refreshes 5/10/15/20 was
425/431/435/435 MB for baseline and 190/191/193/193 MB for the candidate.
Refreshes are deliberately forced in quick succession; this is a churn stress
test, not the daemon's normal 60-second refresh cadence.

Rust allocation counters measure requested live allocation sizes, excluding
native allocations and allocator-retained free pages. Physical footprint comes
from macOS `proc_pid_rusage`. Both builds use the same allocation instrumentation,
so absolute timings include its overhead.

A separate single cold-SQLite-cache run took 4.34 s for baseline and 1.58 s for
the candidate. Filesystem caches were not flushed and baseline ran first; these
observations do not establish a repeatable cold-I/O speedup.

## Correctness and checks

Baseline and candidate produced identical SHA-256 digests for raw activity points,
summary reports, complete detailed reports, and time/project/session/review filters.
Detailed reports also matched for all three cost modes: Auto, Source, and Reprice.
Both reported zero scan warnings. The complete default detailed-report digest was
`c45ad5e47d41cb8739d76dab430fdcee52fe3cbaef624532db0b68ed0c2385d6`.

Release tests passed with `--test-threads=1`: 55 usage tests, 24 Codex source tests,
31 activity tests, and 3 machine usage tests. These filters overlap. Serial execution
avoids the existing progress test's scheduling sensitivity. New coverage checks
stable sorting equivalence and reused buffers across repeated, growing, and empty
assemblies.

`cargo fmt --check` and strict `mbx clippy -- -D warnings` passed. PR preparation
also includes an equivalent match-guard cleanup in session-kind inference to
resolve a pre-existing Clippy warning under Rust 1.95.

## Reproduction

Copy the same frozen Codex corpus into an isolated directory and use a separate
SQLite cache. Put `examples/usage_cache_bench.rs` into the baseline checkout too.
Build baseline normally; build the candidate example with its visitor adapter:

```sh
# Baseline checkout
mbx build --release --example usage_cache_bench

# Candidate checkout; the cfg affects only the example, not library dependencies
mbx rustc --release --example usage_cache_bench -- --cfg memex_native_usage_visitor

# Run each binary against the same frozen corpus/cache; first prime the SQLite cache
env CODEX_HOME=/tmp/frozen-codex target/release/examples/usage_cache_bench /tmp/usage-bench.sqlite3 --stream
env CODEX_HOME=/tmp/frozen-codex target/release/examples/usage_cache_bench /tmp/usage-bench.sqlite3 --stream --refreshes=20
env CODEX_HOME=/tmp/frozen-codex target/release/examples/usage_cache_bench /tmp/usage-bench.sqlite3 --verify
env CODEX_HOME=/tmp/frozen-codex target/release/examples/usage_cache_bench /tmp/usage-bench.sqlite3 --no-memo
```

Omit `--stream` to measure the legacy point-vector API. `--verify` prints only
digests and counts, never detailed records. The benchmark never installs or
restarts a daemon.
