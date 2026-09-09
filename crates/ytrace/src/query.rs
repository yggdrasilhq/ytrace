use crate::YtraceRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Hard cap on records a collector may hold. The scan streams one record at a
/// time; this bounds only the OUTPUT side (an incident list, a tail window).
/// 50k records at ~1-2 kB parsed each is tens of MB — flat regardless of how
/// much history the corpus retains. Before this existed, every verb
/// materialized its whole input window first (measured 4.0 GiB peak RSS for a
/// single wide `query` on an 82 MB corpus — the machine-killer of 2026-09-09).
pub const MAX_COLLECTED_RECORDS: usize = 50_000;

/// Per-probe duration sample cap behind the percentiles. Counts, totals and
/// max are EXACT over the whole window; p50/p95 above this many samples are
/// reservoir-sampled (uniform, deterministic) — honest to well under a
/// percentile point at 4k samples, and the memory per probe stops growing.
const RESERVOIR_CAP: usize = 4096;

/// Summary of one probe kind, like `server perf-summary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSummary {
    pub app: String,
    pub category: String,
    pub name: String,
    pub clock: String,
    pub is_span: bool,
    pub count: u64,
    pub total_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeseriesBucket {
    pub bucket_start_ms: u128,
    pub bucket_end_ms: u128,
    pub count: u64,
    pub span_count: u64,
    pub total_duration_ms: f64,
    pub p95_ms: f64,
    pub incident_count: u64,
}

/// Bounded uniform sample of one probe's durations (Algorithm R, deterministic
/// so two runs over the same corpus agree).
struct Reservoir {
    buf: Vec<f64>,
    seen: u64,
}

impl Reservoir {
    fn new() -> Self {
        Reservoir {
            buf: Vec::new(),
            seen: 0,
        }
    }

    fn observe(&mut self, x: f64) {
        self.seen += 1;
        if self.buf.len() < RESERVOIR_CAP {
            self.buf.push(x);
            return;
        }
        // SplitMix-style scramble of the arrival index — sampling quality is
        // irrelevant here, determinism and uniformity-over-the-window matter.
        let mut z = self.seen.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let idx = ((z ^ (z >> 31)) % self.seen) as usize;
        if idx < RESERVOIR_CAP {
            self.buf[idx] = x;
        }
    }

    fn sorted(&self) -> Vec<f64> {
        let mut v = self.buf.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v
    }
}

// ── the scan primitive ──────────────────────────────────────────────────────
//
// Every reader verb is a fold over the records in a window. The fold used to
// be spelled "collect everything into a Vec, then think" — which made every
// verb's peak memory O(window corpus parsed) and turned the retention raises
// (8 MiB -> 100 MiB live, 1 -> 4 GiB generations) into a fleet-killer. The
// scan below is the ONLY thing allowed to read trace files: one line, one
// parse, one hand-off, nothing retained. Filtering happens before the
// hand-off; collectors keep only what their output needs.

/// Visitor verdict: `Stop` ends the whole scan early (output cap reached).
#[derive(Clone, Copy, PartialEq)]
enum Flow {
    Keep,
    Stop,
}

fn scan_file(
    path: &Path,
    since_ms: Option<u128>,
    f: &mut dyn FnMut(YtraceRecord) -> Flow,
) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    for line in BufReader::new(file).lines().flatten() {
        let parsed = match serde_json::from_str::<YtraceRecord>(&line) {
            Ok(r) => Some(r),
            Err(_) => serde_json::from_str::<Value>(&line)
                .ok()
                // try compat: yggterm perf/event trace shape
                .and_then(|v| crate::compat::try_from_yggterm_value(&v)),
        };
        let Some(r) = parsed else { continue };
        if let Some(since) = since_ms {
            if r.ts_ms < since {
                continue;
            }
        }
        if f(r) == Flow::Stop {
            return true;
        }
    }
    false
}

/// Generations of `home` whose rotation stamp is at/after `floor`, any order.
/// A generation named `ytrace.g<ts>.jsonl` was rotated at `ts` and therefore
/// holds only records OLDER than `ts` — one whose ts predates the window floor
/// cannot contain a record inside the window, so it is skippable without
/// reading. Without this, the query tool's cost grows with the whole retained
/// history (the byte budget exists to bound the window, not to invite
/// re-reading all of it every query).
fn in_window_generations(home: &Path, since_ms: Option<u128>) -> Vec<(u128, PathBuf)> {
    let mut gens: Vec<(u128, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(home) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name
                .strip_prefix("ytrace.g")
                .and_then(|r| r.strip_suffix(".jsonl"))
            {
                if let Ok(gen_ts) = rest.parse::<u128>() {
                    if since_ms.is_some_and(|floor| gen_ts < floor) {
                        continue;
                    }
                    gens.push((gen_ts, e.path()));
                }
            }
        }
    }
    gens
}

/// Fold every record in the window through `f`. Returns true when `f` stopped
/// the scan early. Record order is UNSPECIFIED — order-sensitive collectors
/// (tail) must use the newest-first scan below.
fn for_each_record(
    home: &Path,
    since_ms: Option<u128>,
    f: &mut dyn FnMut(YtraceRecord) -> Flow,
) -> bool {
    // `since_ms` is an absolute epoch. A duration here compiles and silently
    // widens the query to all of history, which reads as a plausible number
    // rather than an error — see the note above `EPOCH_FLOOR_MS`.
    debug_assert!(
        !since_ms.is_some_and(looks_like_duration),
        "ytrace::query: since_ms={:?} is duration-shaped, not an epoch. \
         Use query::since_window(Duration) — or rate_per_min() if you want a rate.",
        since_ms
    );
    let live = home.join("ytrace.jsonl");
    if scan_file(&live, since_ms, f) {
        return true;
    }
    for (_, path) in in_window_generations(home, since_ms) {
        if scan_file(&path, since_ms, f) {
            return true;
        }
    }
    false
}

/// Summarize a ytrace file (live + generations) since `since_ms`.
///
/// `since_ms` is an ABSOLUTE epoch-millisecond floor, not a duration. For
/// "the last N", pass [`since_window`]; for a rate, prefer [`rate_per_min`],
/// which cannot be handed the wrong one.
///
/// Streams: one record resident at a time. Counts/totals/max are exact;
/// percentiles above [`RESERVOIR_CAP`] samples per probe are reservoir-sampled.
pub fn summarize(home: &Path, category_filter: Option<&str>, since_ms: Option<u128>) -> Vec<ProbeSummary> {
    struct Agg {
        app: String,
        count: u64,
        total_ms: f64,
        max_ms: f64,
        durs: Reservoir,
    }
    let mut by_probe: std::collections::HashMap<(String, String, String), Agg> =
        std::collections::HashMap::new();
    for_each_record(home, since_ms, &mut |r: YtraceRecord| {
        if let Some(cat) = category_filter {
            if r.category != cat {
                return Flow::Keep;
            }
        }
        let clock = if r.duration_ms.is_some() {
            r.clock.clone()
        } else {
            "point".to_string()
        };
        let key = (r.category, r.name, clock);
        let agg = by_probe.entry(key).or_insert_with(|| Agg {
            app: r.app.clone(),
            count: 0,
            total_ms: 0.0,
            max_ms: 0.0,
            durs: Reservoir::new(),
        });
        agg.count += 1;
        if let Some(dur) = r.duration_ms {
            agg.total_ms += dur;
            if dur > agg.max_ms {
                agg.max_ms = dur;
            }
            agg.durs.observe(dur);
        }
        Flow::Keep
    });

    let mut out: Vec<ProbeSummary> = by_probe
        .into_iter()
        .map(|((category, name, clock), agg)| {
            let sorted = agg.durs.sorted();
            ProbeSummary {
                app: agg.app,
                is_span: !sorted.is_empty(),
                category,
                name,
                clock,
                count: agg.count,
                total_ms: agg.total_ms,
                p50_ms: percentile(&sorted, 0.5),
                p95_ms: percentile(&sorted, 0.95),
                max_ms: agg.max_ms,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.total_ms
            .partial_cmp(&a.total_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.count.cmp(&a.count))
    });
    out
}

/// Produce folded stacks for flamegraphs: `app;component;category;name <sample_value>`
pub fn flamegraph_folded(home: &Path, since_ms: Option<u128>, by_wall_time: bool) -> Vec<(String, u64)> {
    let mut stacks: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for_each_record(home, since_ms, &mut |r: YtraceRecord| {
        let stack = format!("{app};{comp};{cat};{name}",
            app = if r.app.is_empty() { "yggterm" } else { &r.app },
            comp = if r.component.is_empty() { "core" } else { &r.component },
            cat = r.category,
            name = r.name
        );
        let val = if by_wall_time {
            r.duration_ms.map(|d| (d.max(0.1) * 1000.0) as u64).unwrap_or(100)
        } else {
            1
        };
        *stacks.entry(stack).or_default() += val;
        Flow::Keep
    });
    let mut out: Vec<_> = stacks.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// Generate bucketed timeseries for telemetry trends
///
/// Streams: per-bucket aggregates only, so a 1 s bucket over a month of
/// history holds buckets, never records. Interior buckets with no events are
/// emitted as zeros, same as the collect-first shape did.
pub fn timeseries(home: &Path, bucket_ms: u128, since_ms: Option<u128>) -> Vec<TimeseriesBucket> {
    let bucket_ms = bucket_ms.max(1000);
    struct BucketAgg {
        count: u64,
        total_ms: f64,
        incidents: u64,
        durs: Reservoir,
    }
    let mut buckets: std::collections::BTreeMap<u128, BucketAgg> = std::collections::BTreeMap::new();
    for_each_record(home, since_ms, &mut |r: YtraceRecord| {
        let b_start = (r.ts_ms / bucket_ms) * bucket_ms;
        let agg = buckets.entry(b_start).or_insert_with(|| BucketAgg {
            count: 0,
            total_ms: 0.0,
            incidents: 0,
            durs: Reservoir::new(),
        });
        agg.count += 1;
        if let Some(dur) = r.duration_ms {
            agg.total_ms += dur;
            agg.durs.observe(dur);
        }
        if r.payload.get("incident").and_then(|v| v.as_bool()).unwrap_or(false) {
            agg.incidents += 1;
        }
        Flow::Keep
    });

    let mut out = Vec::new();
    let mut next_start = match buckets.keys().next() {
        Some(k) => *k,
        None => return out,
    };
    let last_start = *buckets.keys().next_back().unwrap();
    while next_start <= last_start {
        match buckets.remove(&next_start) {
            Some(agg) => {
                let sorted = agg.durs.sorted();
                out.push(TimeseriesBucket {
                    bucket_start_ms: next_start,
                    bucket_end_ms: next_start + bucket_ms,
                    count: agg.count,
                    span_count: agg.durs.seen,
                    total_duration_ms: agg.total_ms,
                    p95_ms: percentile(&sorted, 0.95),
                    incident_count: agg.incidents,
                });
            }
            None => out.push(TimeseriesBucket {
                bucket_start_ms: next_start,
                bucket_end_ms: next_start + bucket_ms,
                count: 0,
                span_count: 0,
                total_duration_ms: 0.0,
                p95_ms: 0.0,
                incident_count: 0,
            }),
        }
        next_start += bucket_ms;
    }
    out
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 * p).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

/// Last `n` records in the window, optionally filtered by category.
///
/// O(n) memory: a ring over a NEWEST-FIRST file walk. Files are strictly
/// time-ordered across each other — every record in a generation is older
/// than every record in the generation rotated after it, and older than
/// everything in the live file — so once the ring holds `n` records and a
/// file has been fully consumed, every unread file can only hold records
/// older than the ring. No full-window collect, no sort of the corpus.
pub fn tail_where(
    home: &Path,
    n: usize,
    since_ms: Option<u128>,
    category: Option<&str>,
) -> Vec<YtraceRecord> {
    let n = n.min(MAX_COLLECTED_RECORDS).max(1);
    // The n-newest selector: keyed by (ts, arrival seq) so eviction always
    // removes the oldest record regardless of which file it came from. A
    // naive drop-front ring is wrong here — a record arriving from an older
    // generation lands after newer live records and would evict the wrong
    // end (caught by tail_reads_the_newest_n_without_collecting_the_corpus).
    let mut sel: std::collections::BTreeMap<(u128, u64), YtraceRecord> =
        std::collections::BTreeMap::new();
    let mut seq: u64 = 0;
    let push = |sel: &mut std::collections::BTreeMap<(u128, u64), YtraceRecord>,
                    seq: &mut u64,
                    r: YtraceRecord| {
        if let Some(cat) = category {
            if r.category != cat {
                return Flow::Keep;
            }
        }
        *seq += 1;
        sel.insert((r.ts_ms, *seq), r);
        if sel.len() > n {
            sel.pop_first();
        }
        Flow::Keep
    };
    let live = home.join("ytrace.jsonl");
    scan_file(&live, since_ms, &mut |r| push(&mut sel, &mut seq, r));
    if sel.len() < n {
        let mut gens = in_window_generations(home, since_ms);
        gens.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, path) in gens {
            scan_file(&path, since_ms, &mut |r| push(&mut sel, &mut seq, r));
            if sel.len() >= n {
                break;
            }
        }
    }
    let v: Vec<YtraceRecord> = sel.into_values().collect();
    v
}

pub fn tail(home: &Path, n: usize, since_ms: Option<u128>) -> Vec<YtraceRecord> {
    tail_where(home, n, since_ms, None)
}

/// All incidents since `since_ms` — an ABSOLUTE epoch-ms floor, see [`since_window`].
/// (Records where payload.incident == true.) Filtered during the scan; capped
/// at [`MAX_COLLECTED_RECORDS`].
pub fn incidents(home: &Path, since_ms: Option<u128>) -> Vec<YtraceRecord> {
    let mut out = Vec::new();
    for_each_record(home, since_ms, &mut |r: YtraceRecord| {
        if !r.payload.get("incident").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Flow::Keep;
        }
        if out.len() >= MAX_COLLECTED_RECORDS {
            return Flow::Stop;
        }
        out.push(r);
        Flow::Keep
    });
    out
}

/// Health summary — incident counts and hottest probes for an LLM complaint view.
#[derive(Debug, Clone)]
pub struct HealthSummary {
    pub incidents: usize,
    pub warn: usize,
    pub error: usize,
    pub probes: Vec<ProbeSummary>,
}

pub fn health(home: &Path, since_ms: Option<u128>) -> HealthSummary {
    let inc = incidents(home, since_ms);
    let warn = inc
        .iter()
        .filter(|r| r.payload.get("severity").and_then(|v| v.as_str()) == Some("warn"))
        .count();
    let error = inc
        .iter()
        .filter(|r| r.payload.get("severity").and_then(|v| v.as_str()) == Some("error"))
        .count();
    let probes = summarize(home, None, since_ms);
    HealthSummary {
        incidents: inc.len(),
        warn,
        error,
        probes,
    }
}

// ── windows, rates, and the difference between them ─────────────────────────
//
// Every `since_ms` in this module is an ABSOLUTE epoch-millisecond floor, not a
// duration. The two are the same Rust type and read the same at a call site, so
// handing over a duration compiles, runs, and silently widens the query to all
// of recorded history — `Some(300_000)` is not "the last five minutes", it is
// "since 1970-01-01T00:05:00Z".
//
// That mistake does not fail loudly. It produces a plausible number that is a
// LIFETIME TALLY divided by whatever the caller assumed the window was, so it
// climbs with process age, resets on restart, and falls when retention prunes
// the log — three movements that have nothing to do with the thing measured.
// A threshold placed on it arms once and never disarms.
//
// `since_window` and `rate_per_min` exist so the correct call is the short one.

/// Epoch-ms below which a value cannot be a real timestamp (2001-09-09).
///
/// A duration would have to exceed 31 years to reach this, so anything under it
/// arriving in a `since_ms` position is a duration handed over by mistake.
pub const EPOCH_FLOOR_MS: u128 = 1_000_000_000_000;

/// True when a `since_ms` argument is duration-shaped rather than a timestamp.
pub fn looks_like_duration(since_ms: u128) -> bool {
    since_ms < EPOCH_FLOOR_MS
}

/// The absolute epoch-ms floor for "the last `window`" — the conversion every
/// caller of [`summarize`], [`tail`], [`incidents`] and [`health`] must perform.
pub fn since_window(window: std::time::Duration) -> u128 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    now.saturating_sub(window.as_millis())
}

/// A rate, carrying the window it was measured over and the sample it came from.
///
/// Returned instead of a bare `f64` so a consumer cannot render the number
/// without the two facts needed to judge it: how wide the window was, and how
/// many observations landed in it. A rate over one observation is noise, and a
/// rate whose window is the whole log is a tally.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// Observations that fell inside the window.
    pub count: u64,
    /// The window actually measured over.
    pub window: std::time::Duration,
    /// Observations per minute.
    pub per_min: f64,
}

impl Rate {
    /// Human form that keeps the window attached to the number.
    pub fn describe(&self) -> String {
        format!(
            "{:.1}/min ({} over {}s)",
            self.per_min,
            self.count,
            self.window.as_secs()
        )
    }
}

/// Observations per minute for one probe over a real, bounded window.
///
/// Returns `Some(Rate { count: 0, .. })` when the probe exists in this home but
/// was quiet — which is a measured zero. Returns `None` only when the window is
/// degenerate, so a caller can tell "quiet" from "unmeasurable" rather than
/// collapsing both into an all-clear.
pub fn rate_per_min(
    home: &Path,
    category: &str,
    name: &str,
    window: std::time::Duration,
) -> Option<Rate> {
    let minutes = window.as_secs_f64() / 60.0;
    if minutes <= 0.0 {
        return None;
    }
    let summaries = summarize(home, Some(category), Some(since_window(window)));
    let count = summaries
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.count)
        .unwrap_or(0);
    Some(Rate {
        count,
        window,
        per_min: count as f64 / minutes,
    })
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use std::time::Duration;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ytrace-{}-{}", tag, std::process::id()));
        let home = dir.join("app");
        let _ = fs::create_dir_all(&home);
        dir
    }

    fn mk(cat: &str, ts: u128, dur: Option<f64>) -> String {
        let d = match dur {
            Some(d) => format!(r#""duration_ms":{d},"#),
            None => String::new(),
        };
        format!(
            "{{\"v\":1,\"ts_ms\":{ts},\"pid\":1,\"app\":\"a\",\"app_version\":\"0\",\"component\":\"c\",{d}\"category\":\"{cat}\",\"name\":\"n\",\"clock\":\"wall\",\"payload\":{{}}}}\n"
        )
    }

    #[test]
    fn summarize_skips_generations_entirely_outside_the_window() {
        // regression: the query tool's cost grew with ALL retained history —
        // 152 generations (~350MB) scanned for a small window. A generation
        // rotated before the floor provably holds no in-window record.
        let dir = scratch("skip");
        let home = dir.join("app");
        let floor = 1_700_000_000_000u128; // epoch-shaped, not duration-shaped
        // rotated long before the floor: must be skipped without parsing.
        // The record inside carries an IN-WINDOW timestamp — if the skip is
        // ever removed, this record resurfaces and the test fails.
        fs::write(home.join("ytrace.g1699999000000.jsonl"), mk("old", floor + 100, None)).unwrap();
        // rotated after the floor: scanned normally
        fs::write(
            home.join(format!("ytrace.g{}.jsonl", floor + 500)),
            mk("new", floor + 100, None),
        )
        .unwrap();
        let sums = summarize(&home, None, Some(floor));
        let cats: Vec<_> = sums.iter().map(|s| s.category.as_str()).collect();
        assert!(cats.contains(&"new"), "in-window generation is scanned: {sums:?}");
        assert!(!cats.contains(&"old"), "pre-window generation must be skipped, not parsed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duration_in_a_since_slot_is_recognised() {
        // The exact mistake: five minutes passed where an epoch was expected.
        assert!(looks_like_duration(5 * 60_000));
        assert!(looks_like_duration(Duration::from_secs(86_400).as_millis()));
        // Even an implausibly long duration stays under the floor.
        assert!(looks_like_duration(Duration::from_secs(86_400 * 365).as_millis()));
    }

    #[test]
    fn a_real_timestamp_is_not_mistaken_for_one() {
        let now = since_window(Duration::ZERO);
        assert!(!looks_like_duration(now));
        assert!(!looks_like_duration(now - Duration::from_secs(86_400).as_millis()));
    }

    #[test]
    fn since_window_walks_backwards_from_now() {
        let now = since_window(Duration::ZERO);
        let five_min = since_window(Duration::from_secs(300));
        let delta = now.saturating_sub(five_min);
        // 300_000 ms back, allowing for the clock moving between the two calls.
        assert!((299_000..=301_000).contains(&delta), "delta was {delta}");
    }

    #[test]
    fn a_rate_keeps_its_window_and_count() {
        let r = Rate {
            count: 30,
            window: Duration::from_secs(300),
            per_min: 6.0,
        };
        assert_eq!(r.describe(), "6.0/min (30 over 300s)");
    }

    #[test]
    fn a_degenerate_window_is_unmeasurable_not_zero() {
        let home = std::path::Path::new("/nonexistent-ytrace-home");
        assert!(rate_per_min(home, "ui", "block", Duration::ZERO).is_none());
    }

    #[test]
    fn an_absent_probe_reads_as_a_measured_zero() {
        let home = std::path::Path::new("/nonexistent-ytrace-home");
        let r = rate_per_min(home, "ui", "block", Duration::from_secs(300)).unwrap();
        assert_eq!(r.count, 0);
        assert_eq!(r.per_min, 0.0);
        assert_eq!(r.window, Duration::from_secs(300));
    }

    #[test]
    fn tail_reads_the_newest_n_without_collecting_the_corpus() {
        // two generations + live; the newest records live in the live file.
        let dir = scratch("tail");
        let home = dir.join("app");
        let base = 1_700_000_000_000u128;
        fs::write(
            home.join(format!("ytrace.g{}.jsonl", base + 1000)),
            format!("{}{}", mk("old", base + 10, None), mk("old", base + 20, None)),
        )
        .unwrap();
        fs::write(
            home.join("ytrace.jsonl"),
            format!(
                "{}{}{}{}",
                mk("live", base + 1010, None),
                mk("live", base + 1020, None),
                mk("live", base + 1030, None),
                mk("live", base + 1040, None)
            ),
        )
        .unwrap();
        // n smaller than the live file: everything must come from live, newest last.
        let t2 = tail(&home, 2, None);
        assert_eq!(t2.len(), 2);
        assert!(t2.iter().all(|r| r.category == "live"), "tail(2) mixed old generations in: {t2:?}");
        assert_eq!(t2[1].ts_ms, base + 1040);
        // n larger than live: tops up from the NEWEST generation only.
        let t5 = tail(&home, 5, None);
        assert_eq!(t5.len(), 5);
        assert_eq!(t5.first().unwrap().ts_ms, base + 20, "oldest of the newest five");
        assert_eq!(t5.last().unwrap().ts_ms, base + 1040);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_where_filters_during_the_scan_not_after() {
        let dir = scratch("tailwhere");
        let home = dir.join("app");
        let base = 1_700_000_000_000u128;
        let mut body = String::new();
        for i in 0..50 {
            body.push_str(&mk(if i % 2 == 0 { "ui" } else { "other" }, base + i, None));
        }
        fs::write(home.join("ytrace.jsonl"), body).unwrap();
        let got = tail_where(&home, 3, None, Some("ui"));
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|r| r.category == "ui"));
        // the three NEWEST ui records: indices 48, 46, 44 — newest last
        assert_eq!(got[2].ts_ms, base + 48);
        assert_eq!(got[0].ts_ms, base + 44);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarize_stays_exact_on_counts_while_sampling_durations() {
        let dir = scratch("reservoir");
        let home = dir.join("app");
        let base = 1_700_000_000_000u128;
        let mut body = String::new();
        // 10k spans, duration i — exact count and total must survive sampling.
        for i in 0..10_000u128 {
            body.push_str(&mk("ui", base + i, Some(i as f64)));
        }
        fs::write(home.join("ytrace.jsonl"), body).unwrap();
        let sums = summarize(&home, None, None);
        assert_eq!(sums.len(), 1);
        let s = &sums[0];
        assert_eq!(s.count, 10_000, "count is exact regardless of the reservoir");
        assert_eq!(s.max_ms, 9999.0, "max is exact");
        assert_eq!(s.total_ms, (0..10_000u128).map(|i| i as f64).sum::<f64>());
        assert_eq!(s.is_span, true);
        // p50 of 0..9999 ≈ 5000; a uniform reservoir lands within a few percent.
        assert!((4_500.0..=5_500.0).contains(&s.p50_ms), "p50 drifted: {}", s.p50_ms);
        assert!((9_300.0..=10_700.0).contains(&s.p95_ms), "p95 drifted: {}", s.p95_ms);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn incidents_filter_during_scan_and_carry_the_cap() {
        let dir = scratch("incidents");
        let home = dir.join("app");
        let base = 1_700_000_000_000u128;
        let mut body = String::new();
        for i in 0..100u128 {
            let payload = if i % 10 == 0 {
                r#""incident":true"#
            } else {
                r#""incident":false"#
            };
            body.push_str(&format!(
                "{{\"v\":1,\"ts_ms\":{},\"pid\":1,\"app\":\"a\",\"app_version\":\"0\",\"component\":\"c\",\"category\":\"ui\",\"name\":\"n\",\"clock\":\"wall\",\"payload\":{{{}}}}}\n",
                base + i, payload
            ));
        }
        fs::write(home.join("ytrace.jsonl"), body).unwrap();
        let got = incidents(&home, None);
        assert_eq!(got.len(), 10, "only incidents survive the scan");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeseries_emits_zero_rows_for_quiet_interior_buckets() {
        let dir = scratch("timeseries");
        let home = dir.join("app");
        let base = 1_700_000_000_000u128;
        // two records one bucket-width apart in ms terms (bucket maxed to 1s)
        fs::write(
            home.join("ytrace.jsonl"),
            format!("{}{}", mk("ui", base, Some(5.0)), mk("ui", base + 3_000, Some(7.0))),
        )
        .unwrap();
        let series = timeseries(&home, 1_000, None);
        assert_eq!(series.len(), 4, "buckets 0s,1s,2s,3s — quiet interiors included");
        assert_eq!(series[0].count, 1);
        assert_eq!(series[1].count, 0, "interior quiet bucket is a zero row");
        assert_eq!(series[3].count, 1);
        assert_eq!(series[3].total_duration_ms, 7.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cap_stops_the_scan_not_the_machine() {
        // More matching incidents than MAX_COLLECTED_RECORDS: the collector
        // stops at the cap instead of holding the corpus.
        let dir = scratch("cap");
        let home = dir.join("app");
        let base = 1_700_000_000_000u128;
        let mut body = String::new();
        for i in 0..(MAX_COLLECTED_RECORDS as u128 + 500) {
            body.push_str(&format!(
                "{{\"v\":1,\"ts_ms\":{},\"pid\":1,\"app\":\"a\",\"app_version\":\"0\",\"component\":\"c\",\"category\":\"ui\",\"name\":\"n\",\"clock\":\"wall\",\"payload\":{{\"incident\":true}}}}\n",
                base + i
            ));
        }
        fs::write(home.join("ytrace.jsonl"), body).unwrap();
        let got = incidents(&home, None);
        assert_eq!(got.len(), MAX_COLLECTED_RECORDS, "capped, not unbounded");
        let _ = fs::remove_dir_all(&dir);
    }
}
