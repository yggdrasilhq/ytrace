use crate::YtraceRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

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

/// Summarize a ytrace file (live + generations) since `since_ms`.
///
/// `since_ms` is an ABSOLUTE epoch-millisecond floor, not a duration. For
/// "the last N", pass [`since_window`]; for a rate, prefer [`rate_per_min`],
/// which cannot be handed the wrong one.
pub fn summarize(home: &Path, category_filter: Option<&str>, since_ms: Option<u128>) -> Vec<ProbeSummary> {
    let mut records = Vec::new();
    collect_records(home, since_ms, &mut records);
    let mut by_probe_durs: std::collections::BTreeMap<(String, String, String), Vec<f64>> = std::collections::BTreeMap::new();
    let mut by_probe_counts: std::collections::BTreeMap<(String, String, String), u64> = std::collections::BTreeMap::new();
    let mut app_by_probe: std::collections::HashMap<(String, String, String), String> = std::collections::HashMap::new();

    for r in records {
        if let Some(cat) = category_filter {
            if r.category != cat {
                continue;
            }
        }
        let clock = if r.duration_ms.is_some() {
            r.clock.clone()
        } else {
            "point".to_string()
        };
        let key = (r.category.clone(), r.name.clone(), clock);
        *by_probe_counts.entry(key.clone()).or_default() += 1;
        if let Some(dur) = r.duration_ms {
            by_probe_durs.entry(key.clone()).or_default().push(dur);
        }
        app_by_probe.entry(key).or_insert(r.app.clone());
    }

    let mut out = Vec::new();
    for ((category, name, clock), count) in by_probe_counts {
        let is_span = by_probe_durs.contains_key(&(category.clone(), name.clone(), clock.clone()));
        let (total_ms, p50_ms, p95_ms, max_ms) = if let Some(mut durs) = by_probe_durs.remove(&(category.clone(), name.clone(), clock.clone())) {
            durs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let tot: f64 = durs.iter().sum();
            let p50 = percentile(&durs, 0.5);
            let p95 = percentile(&durs, 0.95);
            let max = durs.last().copied().unwrap_or(0.0);
            (tot, p50, p95, max)
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };
        let app = app_by_probe
            .get(&(category.clone(), name.clone(), clock.clone()))
            .cloned()
            .unwrap_or_default();
        out.push(ProbeSummary {
            app,
            category,
            name,
            clock,
            is_span,
            count,
            total_ms,
            p50_ms,
            p95_ms,
            max_ms,
        });
    }
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
    let mut records = Vec::new();
    collect_records(home, since_ms, &mut records);
    let mut stacks: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for r in records {
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
    }
    let mut out: Vec<_> = stacks.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// Generate bucketed timeseries for telemetry trends
pub fn timeseries(home: &Path, bucket_ms: u128, since_ms: Option<u128>) -> Vec<TimeseriesBucket> {
    let mut records = Vec::new();
    collect_records(home, since_ms, &mut records);
    records.sort_by_key(|r| r.ts_ms);

    if records.is_empty() {
        return Vec::new();
    }
    let bucket_ms = bucket_ms.max(1000);
    let first_ts = records.first().map(|r| r.ts_ms).unwrap_or(0);
    let last_ts = records.last().map(|r| r.ts_ms).unwrap_or(0);

    let mut buckets: std::collections::BTreeMap<u128, Vec<&YtraceRecord>> = std::collections::BTreeMap::new();
    let mut cur = (first_ts / bucket_ms) * bucket_ms;
    while cur <= last_ts {
        buckets.insert(cur, Vec::new());
        cur += bucket_ms;
    }

    for r in &records {
        let b_start = (r.ts_ms / bucket_ms) * bucket_ms;
        buckets.entry(b_start).or_default().push(r);
    }

    let mut out = Vec::new();
    for (start, recs) in buckets {
        let count = recs.len() as u64;
        let mut durs: Vec<f64> = recs.iter().filter_map(|r| r.duration_ms).collect();
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let span_count = durs.len() as u64;
        let total_duration_ms: f64 = durs.iter().sum();
        let p95_ms = percentile(&durs, 0.95);
        let incident_count = recs.iter().filter(|r| r.payload.get("incident").and_then(|v| v.as_bool()).unwrap_or(false)).count() as u64;
        out.push(TimeseriesBucket {
            bucket_start_ms: start,
            bucket_end_ms: start + bucket_ms,
            count,
            span_count,
            total_duration_ms,
            p95_ms,
            incident_count,
        });
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

pub fn tail(home: &Path, n: usize, since_ms: Option<u128>) -> Vec<YtraceRecord> {
    let mut records = Vec::new();
    collect_records(home, since_ms, &mut records);
    records.sort_by_key(|r| r.ts_ms);
    if records.len() > n {
        records.split_off(records.len() - n)
    } else {
        records
    }
}

fn collect_records(home: &Path, since_ms: Option<u128>, out: &mut Vec<YtraceRecord>) {
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
    read_one(&live, since_ms, out);
    // generations. A generation named `ytrace.g<ts>.jsonl` was rotated at `ts`
    // and therefore holds only records OLDER than `ts` — a generation whose ts
    // predates the window floor cannot contain a record inside the window, so
    // it is skippable without reading. Without this, the query tool's cost
    // grows with the whole retained history (the byte budget exists to bound
    // the window, not to invite re-reading all of it every query).
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
                }
                read_one(&e.path(), since_ms, out);
            }
        }
    }
}

/// All incidents since `since_ms` — an ABSOLUTE epoch-ms floor, see [`since_window`].
/// (Records where payload.incident == true.)
pub fn incidents(home: &Path, since_ms: Option<u128>) -> Vec<YtraceRecord> {
    let mut records = Vec::new();
    collect_records(home, since_ms, &mut records);
    records
        .into_iter()
        .filter(|r| r.payload.get("incident").and_then(|v| v.as_bool()).unwrap_or(false))
        .collect()
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

fn read_one(path: &Path, since_ms: Option<u128>, out: &mut Vec<YtraceRecord>) {
    let Ok(f) = fs::File::open(path) else {
        return;
    };
    for line in BufReader::new(f).lines().flatten() {
        if let Ok(r) = serde_json::from_str::<YtraceRecord>(&line) {
            if let Some(since) = since_ms {
                if r.ts_ms < since {
                    continue;
                }
            }
            out.push(r);
        } else if let Ok(v) = serde_json::from_str::<Value>(&line) {
            // try compat: yggterm perf/event trace shape
            if let Some(r) = crate::compat::try_from_yggterm_value(&v) {
                if let Some(since) = since_ms {
                    if r.ts_ms < since {
                        continue;
                    }
                }
                out.push(r);
            }
        }
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

    #[test]
    fn summarize_skips_generations_entirely_outside_the_window() {
        // regression: the query tool's cost grew with ALL retained history —
        // 152 generations (~350MB) scanned for a small window. A generation
        // rotated before the floor provably holds no in-window record.
        let dir = std::env::temp_dir().join(format!("ytrace-query-skip-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let home = dir.join("app");
        let _ = fs::create_dir_all(&home);
        let floor = 1_700_000_000_000u128; // epoch-shaped, not duration-shaped
        let mk = |cat: &str, ts: u128| {
            format!(
                "{{\"v\":1,\"ts_ms\":{ts},\"pid\":1,\"app\":\"a\",\"app_version\":\"0\",\"component\":\"c\",\"category\":\"{cat}\",\"name\":\"n\",\"clock\":\"wall\",\"payload\":{{}}}}\n"
            )
        };
        // rotated long before the floor: must be skipped without parsing.
        // The record inside carries an IN-WINDOW timestamp — if the skip is
        // ever removed, this record resurfaces and the test fails.
        fs::write(home.join("ytrace.g1699999000000.jsonl"), mk("old", floor + 100)).unwrap();
        // rotated after the floor: scanned normally
        fs::write(
            home.join(format!("ytrace.g{}.jsonl", floor + 500)),
            mk("new", floor + 100),
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
}
