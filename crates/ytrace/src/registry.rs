use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub app: String,
    pub pid: u32,
    pub version: String,
    pub home: String,
    pub socket: Option<String>,
    pub ts_ms: u128,
    pub probes: Vec<String>,
}

fn registry_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("ytrace").join("registry.jsonl");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("ytrace")
            .join("registry.jsonl");
    }
    PathBuf::from("/tmp/ytrace/registry.jsonl")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

pub fn heartbeat(app: &str, version: &str, home: &Path, socket: Option<&Path>) {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let entry = RegistryEntry {
        app: app.to_string(),
        pid: std::process::id(),
        version: version.to_string(),
        home: home.to_string_lossy().to_string(),
        socket: socket.map(|p| p.to_string_lossy().to_string()),
        ts_ms: now_ms(),
        probes: Vec::new(),
    };
    if let Ok(line) = serde_json::to_vec(&entry) {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(&line);
            let _ = f.write_all(b"\n");
        }
    }
}

pub fn list(stale_ms: u128) -> Vec<RegistryEntry> {
    let path = registry_path();
    let Ok(f) = fs::File::open(&path) else {
        return Vec::new();
    };
    let now = now_ms();
    let mut map: std::collections::HashMap<String, RegistryEntry> = std::collections::HashMap::new();
    for line in BufReader::new(f).lines().flatten() {
        if let Ok(e) = serde_json::from_str::<RegistryEntry>(&line) {
            // stale check
            if now.saturating_sub(e.ts_ms) > stale_ms {
                continue;
            }
            let key = format!("{}:{}", e.app, e.pid);
            map.insert(key, e);
        }
    }
    let mut out: Vec<_> = map.into_values().collect();
    out.sort_by(|a, b| a.app.cmp(&b.app).then(a.pid.cmp(&b.pid)));
    out
}

pub fn discover_app(app: &str, stale_ms: u128) -> Option<RegistryEntry> {
    list(stale_ms).into_iter().find(|e| e.app == app)
}
