use crate::YtraceRecord;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Summary of one probe kind, like `server perf-summary`.
#[derive(Debug, Clone)]
pub struct ProbeSummary {
    pub app: String,
    pub category: String,
    pub name: String,
    pub clock: String,
    pub count: u64,
    pub total_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

/// Summarize a ytrace file (live + generations) since `since_ms`.
pub fn summarize(home: &Path, category_filter: Option<&str>, since_ms: Option<u128>) -> Vec<ProbeSummary> {
    let mut records = Vec::new();
    collect_records(home, since_ms, &mut records);
    let mut by_probe: std::collections::BTreeMap<(String, String, String), Vec<f64>> = std::collections::BTreeMap::new();
    let mut app_by_probe: std::collections::HashMap<(String, String, String), String> = std::collections::HashMap::new();
    let mut clock_by_probe: std::collections::HashMap<(String, String, String), String> = std::collections::HashMap::new();
    for r in records {
        if let Some(cat) = category_filter {
            if r.category != cat {
                continue;
            }
        }
        if r.duration_ms.is_none() {
            continue;
        }
        let key = (r.category.clone(), r.name.clone(), r.clock.clone());
        by_probe
            .entry(key.clone())
            .or_default()
            .push(r.duration_ms.unwrap());
        app_by_probe.entry(key.clone()).or_insert(r.app.clone());
        clock_by_probe.entry(key.clone()).or_insert(r.clock.clone());
    }
    let mut out = Vec::new();
    for ((category, name, clock), mut durs) in by_probe {
        durs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let count = durs.len() as u64;
        let total_ms: f64 = durs.iter().sum();
        let p50_ms = percentile(&durs, 0.5);
        let p95_ms = percentile(&durs, 0.95);
        let max_ms = durs.last().copied().unwrap_or(0.0);
        let app = app_by_probe
            .get(&(category.clone(), name.clone(), clock.clone()))
            .cloned()
            .unwrap_or_default();
        out.push(ProbeSummary {
            app,
            category,
            name,
            clock,
            count,
            total_ms,
            p50_ms,
            p95_ms,
            max_ms,
        });
    }
    out.sort_by(|a, b| b.total_ms.partial_cmp(&a.total_ms).unwrap());
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
    let live = home.join("ytrace.jsonl");
    read_one(&live, since_ms, out);
    // generations
    if let Ok(entries) = fs::read_dir(home) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("ytrace.g") && name.ends_with(".jsonl") {
                read_one(&e.path(), since_ms, out);
            }
        }
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
