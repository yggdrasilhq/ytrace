use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

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
    /// Attach a script clause to a live provider (durable until detach).
    ///
    /// Grammar: `category/name [where EXPR] [-> @agg(EXPR), ...] [by PATH, ...] [keep PATH, ...] [ring N]`
    Attach {
        #[arg(long, default_value = "yggterm")]
        app: String,
        /// The clause, e.g. `render/gui where duration_ms > 16 -> @quantize(duration_ms)`
        script: String,
        /// Script id (default derived from probe + first aggregate).
        #[arg(long)]
        id: Option<String>,
        /// Attach to one specific pid instead of every live provider of the app.
        #[arg(long)]
        pid: Option<u32>,
        /// Poll drain every N seconds and print a compact rate line.
        #[arg(long)]
        watch: Option<u64>,
        /// Reset counters after each drain (true rate views).
        #[arg(long, default_value_t = false)]
        reset: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Detach a script.
    Detach {
        #[arg(long, default_value = "yggterm")]
        app: String,
        id: String,
        #[arg(long)]
        pid: Option<u32>,
    },
    /// List attached scripts + their anti-false-zero counters.
    Scripts {
        #[arg(long, default_value = "yggterm")]
        app: String,
        #[arg(long)]
        pid: Option<u32>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Drain a script's aggregates (rides the socket, never the byte budget).
    Drain {
        #[arg(long, default_value = "yggterm")]
        app: String,
        id: String,
        /// Zero the counters after reading (atomic vs emitters).
        #[arg(long, default_value_t = false)]
        reset: bool,
        #[arg(long)]
        pid: Option<u32>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

use ytrace::control::request;

fn request_ok(sock: &Path, req: &serde_json::Value) -> Result<serde_json::Value> {
    request(sock, req).map_err(|e| anyhow::anyhow!(e))
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
        Commands::Attach { app, script, id, pid, watch, reset, json } => {
            let targets = targets_for(&app, pid);
            if targets.is_empty() {
                anyhow::bail!("no live provider for `{app}` — is the app running?");
            }
            // A dead advertised socket (a CLI client that exited between its
            // heartbeat and our connect) must not silence the other targets:
            // report per-target and keep going.
            let mut attached = 0usize;
            for t in &targets {
                let req = match &id {
                    Some(id) => serde_json::json!({"verb":"attach","id":id,"script":script}),
                    None => serde_json::json!({"verb":"attach","script":script}),
                };
                let resp = match ytrace::control::request(&t.sock, &req) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("⚠ {} pid={}: unreachable ({e}) — skipped", t.app, t.pid);
                        continue;
                    }
                };
                let derived = resp.get("id").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                if json {
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                } else if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    attached += 1;
                    println!(
                        "attached `{}` → {} pid={}{}",
                        derived,
                        t.app,
                        t.pid,
                        if resp.get("replaced").and_then(|v| v.as_bool()).unwrap_or(false) { " (replaced, aggregates reset)" } else { "" }
                    );
                } else {
                    anyhow::bail!("attach failed: {}", resp.get("error").and_then(|v| v.as_str()).unwrap_or("?"));
                }
                if watch.is_some() {
                    watch_loop(&t.sock, &derived, watch.unwrap_or(2), reset, json)?;
                }
            }
            if attached == 0 {
                anyhow::bail!("no reachable provider accepted the script for `{app}`");
            }
        }
        Commands::Detach { app, id, pid } => {
            for t in targets_for(&app, pid) {
                let resp = request_ok(&t.sock, &serde_json::json!({"verb":"detach","id":id}))?;
                if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    println!("detached `{id}` from {} pid={} (existed: {})", t.app, t.pid,
                        resp.get("existed").and_then(|v| v.as_bool()).unwrap_or(false));
                }
            }
        }
        Commands::Scripts { app, pid, json } => {
            for t in targets_for(&app, pid) {
                let resp = match ytrace::control::request(&t.sock, &serde_json::json!({"verb":"scripts"})) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("⚠ {} pid={}: unreachable ({e}) — skipped", t.app, t.pid);
                        continue;
                    }
                };
                if json {
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                    continue;
                }
                println!("{} pid={}:", t.app, t.pid);
                if let Some(scripts) = resp.get("scripts").and_then(|v| v.as_array()) {
                    if scripts.is_empty() {
                        println!("  (no scripts attached)");
                    }
                    for s in scripts {
                        let st = &s["stats"];
                        println!(
                            "  {:<28} {}",
                            s["id"].as_str().unwrap_or("?"),
                            s["script"].as_str().unwrap_or("?")
                        );
                        println!(
                            "    fired={} matched={} schema_miss={} overflow_groups={} ring_dropped={}",
                            st["fired"], st["matched"], st["schema_miss"],
                            st["overflow_groups"], st["ring_dropped"]
                        );
                    }
                }
            }
        }
        Commands::Drain { app, id, reset, pid, json } => {
            for t in targets_for(&app, pid) {
                let req = serde_json::json!({"verb":"drain","id":id,"reset":reset});
                let resp = match ytrace::control::request(&t.sock, &req) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("⚠ {} pid={}: unreachable ({e}) — skipped", t.app, t.pid);
                        continue;
                    }
                };
                if json {
                    println!("{}", serde_json::to_string_pretty(&resp)?);
                    continue;
                }
                if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    print_snapshot(&resp["snapshot"]);
                } else {
                    anyhow::bail!("{}", resp.get("error").and_then(|v| v.as_str()).unwrap_or("?"));
                }
            }
        }
    }
    Ok(())
}

struct Target {
    app: String,
    pid: u32,
    sock: PathBuf,
}

/// Live providers of `app` from the registry — never file-path guessing.
fn targets_for(app: &str, pid: Option<u32>) -> Vec<Target> {
    ytrace::registry::list(45_000)
        .into_iter()
        .filter(|e| e.app == app)
        .filter(|e| pid.is_none_or(|p| e.pid == p))
        .filter_map(|e| {
            let s = e.socket?;
            if s.is_empty() {
                return None;
            }
            Some(Target { app: e.app, pid: e.pid, sock: PathBuf::from(s) })
        })
        .collect()
}

fn print_snapshot(snap: &serde_json::Value) {
    let st = &snap["stats"];
    println!("== {}  ({})", snap["id"].as_str().unwrap_or("?"), snap["probe"].as_str().unwrap_or("?"));
    println!("   {}", serde_json::to_string(st).unwrap_or_default());
    if let Some(groups) = snap["groups"].as_array() {
        for g in groups {
            let key = match &g["key"] {
                serde_json::Value::Null => String::new(),
                k => format!(" [{}]", k),
            };
            let mut parts = vec![format!("count={}", g["count"])];
            for f in ["sum", "avg", "min", "max"] {
                if let Some(v) = g.get(f) {
                    if let Some(n) = v.as_f64() {
                        parts.push(format!("{f}={n:.3}"));
                    }
                }
            }
            if let Some(q) = g.get("quantize") {
                parts.push(format!(
                    "p50={:?} p95={:?} max={:?}",
                    q["p50"].as_f64(),
                    q["p95"].as_f64(),
                    q["max"].as_f64()
                ));
            }
            println!("   {}{}", key, parts.join(" "));
        }
    }
    if let Some(ring) = snap["ring"].as_array() {
        println!("   ring ({}):", ring.len());
        for r in ring.iter().rev().take(5) {
            println!("     {}", serde_json::to_string(r).unwrap_or_default());
        }
    }
}

fn watch_loop(sock: &Path, id: &str, every_secs: u64, reset: bool, json: bool) -> Result<()> {
    let req = serde_json::json!({"verb":"drain","id":id,"reset":reset});
    loop {
        std::thread::sleep(std::time::Duration::from_secs(every_secs));
        let resp = match ytrace::control::request(sock, &req) {
            Ok(r) => r,
            Err(_) => break, // process died — stop watching
        };
        if json {
            println!("{}", serde_json::to_string(&resp["snapshot"])?);
        } else {
            let s = &resp["snapshot"]["stats"];
            let interval = every_secs as f64;
            println!(
                "[watch] fired/s={:.1} matched/s={:.1} schema_miss={} groups={} ring={}",
                s["fired"].as_f64().unwrap_or(0.0) / interval,
                s["matched"].as_f64().unwrap_or(0.0) / interval,
                s["schema_miss"],
                s["groups"],
                s.get("ring_dropped").map(|v| v.to_string()).unwrap_or_default(),
            );
        }
    }
    Ok(())
}
