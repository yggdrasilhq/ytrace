//! Compat shim: read yggterm's existing `perf-telemetry.jsonl` / `event-trace.jsonl`
//! as if they were ytrace records, so old and new readers share bytes during migration.

use crate::{YtraceRecord, YTRACE_WIRE_VERSION};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Where yggterm currently writes its telemetry (pre-ytrace).
pub fn yggterm_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("YGGTERM_HOME") {
        return Some(PathBuf::from(dir));
    }
    if let Ok(home) = std::env::var("HOME") {
        // yggterm default: ~/.yggterm
        let p = PathBuf::from(home).join(".yggterm");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn yggterm_perf_path(home: &Path) -> PathBuf {
    home.join("perf-telemetry.jsonl")
}
pub fn yggterm_trace_path(home: &Path) -> PathBuf {
    home.join("event-trace.jsonl")
}

/// Try to interpret a generic JSON value from yggterm's old streams as a ytrace record.
pub fn try_from_yggterm_value(v: &Value) -> Option<YtraceRecord> {
    // yggterm perf event shape:
    // {"ts_ms":..., "pid":..., "category":"daemon_request","name":"status","payload":{"duration_ms":1.23,"meta":{...}}}
    // or trace shape:
    // {"ts_ms":..., "pid":..., "component":"daemon","category":"trace","name":"something","payload":{...}}
    let ts_ms = v.get("ts_ms")?.as_u64()? as u128;
    let pid = v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let category = v
        .get("category")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let component = v
        .get("component")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let payload = v.get("payload").cloned().unwrap_or(Value::Null);
    // perf shape nests duration_ms inside payload
    let duration_ms = payload
        .get("duration_ms")
        .and_then(|x| x.as_f64())
        .or_else(|| v.get("duration_ms").and_then(|x| x.as_f64()));
    let clock = if category == "render" { "cpu" } else { "wall" };
    Some(YtraceRecord {
        v: YTRACE_WIRE_VERSION,
        ts_ms,
        pid,
        app: "yggterm".to_string(),
        app_version: "compat".to_string(),
        component,
        category,
        name,
        clock: clock.to_string(),
        duration_ms,
        payload,
    })
}

/// Resolve a home for an app: prefer YTRACE_HOME/<app>, then yggterm compat, then default.
pub fn resolve_home(app: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("YTRACE_HOME") {
        return PathBuf::from(dir).join(app);
    }
    if app == "yggterm" {
        if let Some(h) = yggterm_home() {
            return h;
        }
    }
    // fallback: XDG or ~/.local/share
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("ytrace").join(app);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("ytrace")
            .join(app);
    }
    PathBuf::from("/tmp/ytrace").join(app)
}
