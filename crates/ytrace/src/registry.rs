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

/// How often a provider re-announces itself. The spec's number, now actually
/// enforced: `heartbeat` is called from the emit path, so without this gate it
/// fired once per RECORD rather than once per interval, and the discovery index
/// grew at the rate of the whole event stream.
pub const HEARTBEAT_INTERVAL_MS: u128 = 15_000;

/// Compaction ceiling for the registry file.
///
/// ⛔ This file lives under `$XDG_RUNTIME_DIR`, which is a **tmpfs** — every
/// byte is resident memory. The trace streams have had generational retention
/// since they were written; the index that exists to *find* them had none, so
/// the one unbounded thing in the system was the small one nobody looked at.
/// Measured before this bound: hundreds of megabytes of RAM and 2.3 M lines, to
/// answer a question whose answer is a handful of rows.
pub const REGISTRY_MAX_BYTES: u64 = 256 * 1024;

static LAST_HEARTBEAT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn heartbeat(app: &str, version: &str, home: &Path, socket: Option<&Path>) {
    heartbeat_with_probes(app, version, home, socket, &[]);
}

/// Announce this provider, including the probes it has registered.
///
/// The probe list is what makes the registry a discovery surface rather than a
/// liveness list: a consumer that cannot ask "what can this provider tell me"
/// has to hard-code probe names, which is a second copy of the provider's own
/// declaration.
pub fn heartbeat_with_probes(
    app: &str,
    version: &str,
    home: &Path,
    socket: Option<&Path>,
    probes: &[String],
) {
    use std::sync::atomic::Ordering;
    let now = now_ms();
    let last = LAST_HEARTBEAT_MS.load(Ordering::Relaxed) as u128;
    if last != 0 && now.saturating_sub(last) < HEARTBEAT_INTERVAL_MS {
        return;
    }
    // Claim the slot before doing any I/O so concurrent emitters on other
    // threads fall through the gate rather than queueing behind it.
    if LAST_HEARTBEAT_MS
        .compare_exchange(last as u64, now as u64, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

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
        ts_ms: now,
        probes: probes.to_vec(),
    };
    if let Ok(line) = serde_json::to_vec(&entry) {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(&line);
            let _ = f.write_all(b"\n");
        }
    }
    compact_if_needed(&path, REGISTRY_MAX_BYTES, now);
}

/// Rewrite the registry keeping only the newest entry per `(app, pid)`.
///
/// Only the newest line per provider is ever read, so every earlier line is
/// already dead weight — compaction discards history nothing consults. A lost
/// concurrent append costs one heartbeat, which the next interval replaces;
/// the write is temp-plus-rename so a reader never sees a partial file.
pub fn compact_if_needed(path: &Path, max_bytes: u64, now: u128) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    let Ok(f) = fs::File::open(path) else {
        return;
    };
    let mut map: std::collections::HashMap<String, RegistryEntry> = std::collections::HashMap::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if let Ok(e) = serde_json::from_str::<RegistryEntry>(&line) {
            // Keep anything inside a generous multiple of the staleness window:
            // compaction is a size bound, not a liveness decision, and throwing
            // away a provider a reader would still have accepted turns a memory
            // fix into a correctness bug.
            if now.saturating_sub(e.ts_ms) > HEARTBEAT_INTERVAL_MS * 20 {
                continue;
            }
            let key = format!("{}:{}", e.app, e.pid);
            match map.get(&key) {
                Some(prev) if prev.ts_ms >= e.ts_ms => {}
                _ => {
                    map.insert(key, e);
                }
            }
        }
    }
    let tmp = path.with_extension("jsonl.compacting");
    let Ok(mut out) = fs::File::create(&tmp) else {
        return;
    };
    let mut entries: Vec<_> = map.into_values().collect();
    entries.sort_by(|a, b| a.app.cmp(&b.app).then(a.pid.cmp(&b.pid)));
    for e in &entries {
        if let Ok(line) = serde_json::to_vec(e) {
            let _ = out.write_all(&line);
            let _ = out.write_all(b"\n");
        }
    }
    let _ = out.flush();
    drop(out);
    let _ = fs::rename(&tmp, path);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(app: &str, pid: u32, ts_ms: u128) -> RegistryEntry {
        RegistryEntry {
            app: app.to_string(),
            pid,
            version: "0.0.0-test".to_string(),
            home: "/nonexistent/test-home".to_string(),
            socket: None,
            ts_ms,
            probes: vec!["example/probe".to_string()],
        }
    }

    fn write_lines(path: &Path, entries: &[RegistryEntry]) {
        let mut f = fs::File::create(path).expect("create");
        for e in entries {
            f.write_all(&serde_json::to_vec(e).unwrap()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ytrace-registry-test-{name}-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn compaction_keeps_only_the_newest_line_per_provider() {
        let dir = tmpdir("newest");
        let path = dir.join("registry.jsonl");
        let now = 1_000_000u128;
        // One provider announcing itself 500 times, as the un-throttled emit
        // path used to do — only the last one has ever been readable.
        let mut rows: Vec<RegistryEntry> = (0..500)
            .map(|i| entry("alpha", 7, now - 500 + i as u128))
            .collect();
        rows.push(entry("beta", 9, now));
        write_lines(&path, &rows);

        let before = fs::metadata(&path).unwrap().len();
        compact_if_needed(&path, 0, now); // force
        let after = fs::metadata(&path).unwrap().len();

        assert!(after < before, "compaction must shrink the file: {before} -> {after}");
        let kept: Vec<RegistryEntry> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(kept.len(), 2, "one line per (app,pid), not one per heartbeat");
        let alpha = kept.iter().find(|e| e.app == "alpha").expect("alpha survives");
        assert_eq!(alpha.ts_ms, now - 1, "the NEWEST alpha line is the one kept");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_does_nothing_below_the_budget() {
        let dir = tmpdir("under");
        let path = dir.join("registry.jsonl");
        write_lines(&path, &[entry("alpha", 1, 10), entry("alpha", 1, 20)]);
        let before = fs::read_to_string(&path).unwrap();
        compact_if_needed(&path, REGISTRY_MAX_BYTES, 100);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            before,
            "a file inside its budget must not be rewritten — compaction is a size bound"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_drops_long_dead_providers_but_keeps_recent_ones() {
        let dir = tmpdir("stale");
        let path = dir.join("registry.jsonl");
        let now = 10_000_000u128;
        write_lines(&path, &[
            entry("ancient", 1, now - HEARTBEAT_INTERVAL_MS * 100),
            entry("recent", 2, now - HEARTBEAT_INTERVAL_MS * 2),
        ]);
        compact_if_needed(&path, 0, now);
        let kept: Vec<RegistryEntry> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let apps: Vec<_> = kept.iter().map(|e| e.app.as_str()).collect();
        assert!(apps.contains(&"recent"), "a provider a reader would accept must survive");
        assert!(!apps.contains(&"ancient"), "a provider dead 25 minutes is not discoverable");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_leaves_no_temp_file_behind() {
        let dir = tmpdir("tmpfile");
        let path = dir.join("registry.jsonl");
        write_lines(&path, &[entry("alpha", 1, 10)]);
        compact_if_needed(&path, 0, 100);
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("compacting"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
