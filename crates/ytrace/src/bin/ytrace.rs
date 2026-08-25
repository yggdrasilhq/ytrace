use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ytrace", version, about = "ytrace probe bus CLI — query fleet telemetry file-first")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Ranked summary like `server perf-summary` — clock-aware.
    Query {
        #[arg(long, default_value = "yggterm")]
        app: String,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// e.g. 60s, 5m, 1h, 15s — or raw ms number.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 50)]
        top: usize,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Last N events like `server trace tail`.
    Tail {
        #[arg(long, default_value = "yggterm")]
        app: String,
        #[arg(long)]
        category: Option<String>,
        /// N events (default) or --since window.
        #[arg(long)]
        lines: Option<usize>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Durable incidents (payload.incident=true).
    Incidents {
        #[arg(long, default_value = "yggterm")]
        app: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Health rollup: incident counts + hottest probes (LLM complaint view).
    Health {
        #[arg(long, default_value = "yggterm")]
        app: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Top-table view of system and application probes with sorted attribution.
    Top {
        #[arg(long, default_value = "yggterm")]
        app: String,
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 30)]
        top: usize,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Flamegraph folded-stack format for flamegraph viewers.
    Flame {
        #[arg(long, default_value = "yggterm")]
        app: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = false)]
        by_count: bool,
    },
    /// Bucketed timeseries trend analysis.
    Timeseries {
        #[arg(long, default_value = "yggterm")]
        app: String,
        /// Bucket size, e.g. 1s, 5s, 1m
        #[arg(long, default_value = "5s")]
        bucket: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Discovery registry.
    Registry {
        #[arg(long, default_value_t = false)]
        list: bool,
        #[arg(long)]
        stale: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

fn parse_since(s: &str) -> Option<u128> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // bare number => ms
    if let Ok(ms) = s.parse::<u128>() {
        let now = now_ms();
        return Some(now.saturating_sub(ms));
    }
    // with suffix
    let (num_str, mult) = if s.ends_with("ms") {
        (&s[..s.len() - 2], 1u128)
    } else if s.ends_with('s') {
        (&s[..s.len() - 1], 1000u128)
    } else if s.ends_with('m') {
        (&s[..s.len() - 1], 60_000u128)
    } else if s.ends_with('h') {
        (&s[..s.len() - 1], 3_600_000u128)
    } else if s.ends_with('d') {
        (&s[..s.len() - 1], 86_400_000u128)
    } else {
        (s, 1u128)
    };
    if let Ok(n) = num_str.parse::<u128>() {
        let delta = n.saturating_mul(mult);
        Some(now_ms().saturating_sub(delta))
    } else {
        None
    }
}

fn parse_stale(s: &str) -> u128 {
    // stale is a window, not a since — return duration ms
    let s = s.trim();
    if s.is_empty() {
        return 45_000;
    }
    if let Ok(ms) = s.parse::<u128>() {
        return ms;
    }
    let (num_str, mult) = if s.ends_with("ms") {
        (&s[..s.len() - 2], 1u128)
    } else if s.ends_with('s') {
        (&s[..s.len() - 1], 1000u128)
    } else if s.ends_with('m') {
        (&s[..s.len() - 1], 60_000u128)
    } else if s.ends_with('h') {
        (&s[..s.len() - 1], 3_600_000u128)
    } else {
        (s, 1u128)
    };
    num_str.parse::<u128>().unwrap_or(45).saturating_mul(mult)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

fn resolve_home(app: &str) -> PathBuf {
    ytrace::compat::resolve_home(app)
}

fn main() -> Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match cli.command {
        Commands::Query {
            app,
            category,
            name,
            since,
            top,
            json,
        } => {
            let home = resolve_home(&app);
            let since_ms = since.as_deref().and_then(parse_since);
            let mut sums = ytrace::query::summarize(&home, category.as_deref(), since_ms);
            if let Some(n) = name {
                sums.retain(|s| s.name == n);
            }
            sums.truncate(top);
            if json {
                let out: Vec<serde_json::Value> = sums
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "app": s.app,
                            "category": s.category,
                            "name": s.name,
                            "clock": s.clock,
                            "count": s.count,
                            "total_ms": s.total_ms,
                            "p50_ms": s.p50_ms,
                            "p95_ms": s.p95_ms,
                            "max_ms": s.max_ms,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!(
                    "{:<16} {:<18} {:<5} {:>6} {:>10} {:>8} {:>8} {:>8}",
                    "category", "name", "clock", "count", "total_ms", "p50", "p95", "max"
                );
                for s in sums {
                    println!(
                        "{:<16} {:<18} {:<5} {:>6} {:>10.1} {:>8.1} {:>8.1} {:>8.1}",
                        s.category, s.name, s.clock, s.count, s.total_ms, s.p50_ms, s.p95_ms, s.max_ms
                    );
                }
            }
        }
        Commands::Tail {
            app,
            category,
            lines,
            since,
            json,
        } => {
            let home = resolve_home(&app);
            let since_ms = since.as_deref().and_then(parse_since);
            // --lines N dominates; default 20
            let n = lines.unwrap_or(20);
            let recs = if let Some(cat) = category {
                let all = ytrace::query::tail(&home, 100_000, since_ms);
                let mut filtered: Vec<_> = all.into_iter().filter(|r| r.category == cat).collect();
                filtered.sort_by_key(|r| r.ts_ms);
                if filtered.len() > n {
                    filtered.split_off(filtered.len() - n)
                } else {
                    filtered
                }
            } else {
                ytrace::query::tail(&home, n, since_ms)
            };
            if json {
                let out: Vec<serde_json::Value> =
                    recs.iter().map(|r| serde_json::to_value(r).unwrap()).collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for r in recs {
                    println!("{}", serde_json::to_string(&r)?);
                }
            }
        }
        Commands::Incidents { app, since, json } => {
            let home = resolve_home(&app);
            let since_ms = since.as_deref().and_then(parse_since);
            let recs = ytrace::query::incidents(&home, since_ms);
            if json {
                let out: Vec<serde_json::Value> =
                    recs.iter().map(|r| serde_json::to_value(r).unwrap()).collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for r in &recs {
                    println!(
                        "{} {} {}/{} {} {:?}",
                        r.ts_ms, r.app, r.category, r.name, r.clock, r.payload
                    );
                }
                eprintln!("incidents: {}", recs.len());
            }
        }
        Commands::Health { app, since, json } => {
            let home = resolve_home(&app);
            let since_ms = since.as_deref().and_then(parse_since);
            let h = ytrace::query::health(&home, since_ms);
            if json {
                let out = serde_json::json!({
                    "incidents": h.incidents,
                    "warn": h.warn,
                    "error": h.error,
                    "probes": h.probes.iter().map(|s| serde_json::json!({
                        "app": s.app, "category": s.category, "name": s.name,
                        "clock": s.clock, "is_span": s.is_span, "count": s.count, "total_ms": s.total_ms,
                        "p50_ms": s.p50_ms, "p95_ms": s.p95_ms, "max_ms": s.max_ms
                    })).collect::<Vec<_>>()
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("incidents: {} warn:{} error:{} probes:{}", h.incidents, h.warn, h.error, h.probes.len());
                for s in h.probes.iter().take(10) {
                    println!("  {} {} {} {} count={} total={:.1} p50={:.1}", s.app, s.category, s.name, s.clock, s.count, s.total_ms, s.p50_ms);
                }
            }
        }
        Commands::Top {
            app,
            category,
            since,
            top,
            json,
        } => {
            let home = resolve_home(&app);
            let since_ms = since.as_deref().and_then(parse_since);
            let mut sums = ytrace::query::summarize(&home, category.as_deref(), since_ms);
            sums.truncate(top);
            if json {
                println!("{}", serde_json::to_string_pretty(&sums)?);
            } else {
                println!(
                    "┌────────────────────────────────┬──────────────┬────────┬──────────┬──────────┬──────────┬──────────┐"
                );
                println!(
                    "│ {:<30} │ {:<12} │ {:<6} │ {:>8} │ {:>8} │ {:>8} │ {:>8} │",
                    "Probe (category/name)", "App", "Clock", "Count", "Total ms", "p95 ms", "Max ms"
                );
                println!(
                    "├────────────────────────────────┼──────────────┼────────┼──────────┼──────────┼──────────┼──────────┤"
                );
                for s in sums {
                    let probe_name = format!("{}/{}", s.category, s.name);
                    println!(
                        "│ {:<30} │ {:<12} │ {:<6} │ {:>8} │ {:>8.1} │ {:>8.1} │ {:>8.1} │",
                        if probe_name.len() > 30 { &probe_name[..30] } else { &probe_name },
                        if s.app.len() > 12 { &s.app[..12] } else { &s.app },
                        s.clock,
                        s.count,
                        s.total_ms,
                        s.p95_ms,
                        s.max_ms
                    );
                }
                println!(
                    "└────────────────────────────────┴──────────────┴────────┴──────────┴──────────┴──────────┴──────────┘"
                );
            }
        }
        Commands::Flame {
            app,
            since,
            by_count,
        } => {
            let home = resolve_home(&app);
            let since_ms = since.as_deref().and_then(parse_since);
            let stacks = ytrace::query::flamegraph_folded(&home, since_ms, !by_count);
            for (stack, val) in stacks {
                println!("{stack} {val}");
            }
        }
        Commands::Timeseries {
            app,
            bucket,
            since,
            json,
        } => {
            let home = resolve_home(&app);
            let since_ms = since.as_deref().and_then(parse_since);
            let bucket_ms = parse_stale(&bucket);
            let series = ytrace::query::timeseries(&home, bucket_ms, since_ms);
            if json {
                println!("{}", serde_json::to_string_pretty(&series)?);
            } else {
                println!(
                    "{:<24} {:>8} {:>8} {:>10} {:>8} {:>10}",
                    "Bucket Time", "Events", "Spans", "Total ms", "p95 ms", "Incidents"
                );
                for b in series {
                    let dt = chrono::DateTime::from_timestamp_millis(b.bucket_start_ms as i64)
                        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| b.bucket_start_ms.to_string());
                    println!(
                        "{:<24} {:>8} {:>8} {:>10.1} {:>8.1} {:>10}",
                        dt, b.count, b.span_count, b.total_duration_ms, b.p95_ms, b.incident_count
                    );
                }
            }
        }
        Commands::Registry { list: _, stale, json } => {
            let stale_ms = stale.as_deref().map(parse_stale).unwrap_or(45_000);
            let entries = ytrace::registry::list(stale_ms);
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                for e in &entries {
                    println!("{} pid={} ver={} home={} ts={} probes={:?}", e.app, e.pid, e.version, e.home, e.ts_ms, e.probes);
                }
                if entries.is_empty() {
                    eprintln!("(no live providers — stale {}ms)", stale_ms);
                }
            }
        }
    }
    Ok(())
}
