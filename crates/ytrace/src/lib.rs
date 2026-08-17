use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ── wire ────────────────────────────────────────────────────────────────────

pub const YTRACE_WIRE_VERSION: u8 = 1;

/// Which clock a span's `duration_ms` is measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clock {
    Wall,
    Cpu,
}

impl Clock {
    pub fn as_str(self) -> &'static str {
        match self {
            Clock::Wall => "wall",
            Clock::Cpu => "cpu",
        }
    }
}

/// Which probe in which provider.
#[derive(Debug, Clone)]
pub struct Probe {
    pub category: String,
    pub name: String,
    pub clock: Clock,
    pub sample: Sample,
}

/// Sampling policy for a probe.
#[derive(Debug, Clone)]
pub struct Sample {
    pub floor_ms: Option<f64>,
    pub rate: Option<u64>,
}

impl Sample {
    pub const fn always() -> Self {
        Self {
            floor_ms: None,
            rate: None,
        }
    }
    pub const fn noisy() -> Self {
        Self {
            floor_ms: Some(8.0),
            rate: Some(50),
        }
    }
}

// ── record ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YtraceRecord {
    pub v: u8,
    pub ts_ms: u128,
    pub pid: u32,
    pub app: String,
    pub app_version: String,
    pub component: String,
    pub category: String,
    pub name: String,
    pub clock: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(default)]
    pub payload: Value,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

// ── retention (generational, from yggterm-core::retention, trimmed) ─────────

pub const DEFAULT_MAX_AGE_MS: u128 = 3 * 24 * 60 * 60 * 1000; // 3 days ceiling

#[derive(Clone, Copy, Debug)]
pub struct Retention {
    pub live_max_bytes: u64,
    pub generations_max_bytes: u64,
    pub max_age_ms: u128,
}

pub const DEFAULT_RETENTION: Retention = Retention {
    live_max_bytes: 8 * 1024 * 1024,
    generations_max_bytes: 64 * 1024 * 1024,
    max_age_ms: DEFAULT_MAX_AGE_MS,
};

fn generation_path(path: &Path, ts_ms: u128) -> PathBuf {
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".jsonl").unwrap_or(n))
        .unwrap_or("ytrace");
    path.with_file_name(format!("{stem}.g{ts_ms}.jsonl"))
}

fn prune_generations(path: &Path, retention: Retention, now_ms: u128) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(stem) = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.strip_suffix(".jsonl").unwrap_or(n).to_string())
    else {
        return;
    };
    let prefix = format!("{stem}.g");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let mut gens: Vec<(PathBuf, u128, u64)> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix(&prefix).and_then(|r| r.strip_suffix(".jsonl")) {
            if let Ok(ts) = rest.parse::<u128>() {
                if let Ok(meta) = e.metadata() {
                    gens.push((e.path(), ts, meta.len()));
                }
            }
        }
    }
    // age prune
    for (p, ts, _) in gens.iter() {
        if now_ms.saturating_sub(*ts) > retention.max_age_ms {
            let _ = fs::remove_file(p);
        }
    }
    // re-list after age prune
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let mut gens: Vec<(PathBuf, u128, u64)> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix(&prefix).and_then(|r| r.strip_suffix(".jsonl")) {
            if let Ok(ts) = rest.parse::<u128>() {
                if let Ok(meta) = e.metadata() {
                    gens.push((e.path(), ts, meta.len()));
                }
            }
        }
    }
    gens.sort_by_key(|(_, ts, _)| *ts);
    let total: u64 = gens.iter().map(|(_, _, b)| *b).sum();
    let mut total_mut = total;
    for (p, _, b) in gens {
        if total_mut <= retention.generations_max_bytes {
            break;
        }
        let _ = fs::remove_file(&p);
        total_mut = total_mut.saturating_sub(b);
    }
}

fn rotate_if_needed(path: &Path, retention: Retention, incoming: u64) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len().saturating_add(incoming) <= retention.live_max_bytes {
        return;
    }
    let ts = now_ms();
    let gen = generation_path(path, ts);
    let _ = fs::rename(path, &gen);
    prune_generations(path, retention, ts);
}

// ── provider ────────────────────────────────────────────────────────────────

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static YTRACE_ENABLED: AtomicBool = AtomicBool::new(true);
static SAMPLE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn set_enabled(enabled: bool) {
    YTRACE_ENABLED.store(enabled, Ordering::Relaxed);
}
pub fn is_enabled() -> bool {
    YTRACE_ENABLED.load(Ordering::Relaxed)
}

fn should_record(probe: &Probe, duration_ms: f64) -> bool {
    let Some(floor) = probe.sample.floor_ms else {
        return true;
    };
    let Some(rate) = probe.sample.rate else {
        return true;
    };
    if duration_ms >= floor {
        return true;
    }
    SAMPLE_COUNTER.fetch_add(1, Ordering::Relaxed) % rate == 0
}

/// A ytrace provider — one per app process.
pub struct Provider {
    pub app: String,
    pub app_version: String,
    pub home: PathBuf,
    probes: Mutex<HashMap<(String, String), Probe>>,
    retention: Retention,
}

impl Provider {
    pub fn new(app: impl Into<String>, app_version: impl Into<String>) -> Self {
        let app = app.into();
        let home = default_home(&app);
        Self::with_home(app, app_version, home)
    }

    pub fn with_home(
        app: impl Into<String>,
        app_version: impl Into<String>,
        home: impl Into<PathBuf>,
    ) -> Self {
        let home = home.into();
        let _ = fs::create_dir_all(&home);
        Self {
            app: app.into(),
            app_version: app_version.into(),
            home,
            probes: Mutex::new(HashMap::new()),
            retention: DEFAULT_RETENTION,
        }
    }

    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    pub fn register_probe(
        &self,
        category: impl Into<String>,
        name: impl Into<String>,
        clock: Clock,
        sample: Sample,
    ) {
        let category = category.into();
        let name = name.into();
        let mut map = self.probes.lock().unwrap();
        map.insert(
            (category.clone(), name.clone()),
            Probe {
                category,
                name,
                clock,
                sample,
            },
        );
    }

    /// Register a "category/name" probe in one string.
    pub fn register(&self, slash: &str, clock: Clock, sample: Sample) {
        let (cat, name) = slash
            .split_once('/')
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .unwrap_or((slash.to_string(), slash.to_string()));
        self.register_probe(cat, name, clock, sample);
    }

    fn probe_for(&self, category: &str, name: &str) -> Option<Probe> {
        self.probes
            .lock()
            .unwrap()
            .get(&(category.to_string(), name.to_string()))
            .cloned()
    }

    fn resolve_probe(&self, category: &str, name: &str) -> Probe {
        self.probe_for(category, name).unwrap_or(Probe {
            category: category.to_string(),
            name: name.to_string(),
            clock: Clock::Wall,
            sample: Sample::always(),
        })
    }

    pub fn event(
        &self,
        component: impl Into<String>,
        category: impl Into<String>,
        name: impl Into<String>,
        payload: Value,
    ) {
        if !is_enabled() {
            return;
        }
        let category = category.into();
        let name = name.into();
        let rec = YtraceRecord {
            v: YTRACE_WIRE_VERSION,
            ts_ms: now_ms(),
            pid: std::process::id(),
            app: self.app.clone(),
            app_version: self.app_version.clone(),
            component: component.into(),
            category,
            name,
            clock: Clock::Wall.as_str().to_string(),
            duration_ms: None,
            payload,
        };
        self.append(&rec);
    }

    pub fn span(&self, component: impl Into<String>, category: impl Into<String>, name: impl Into<String>, ctx: Value) -> SpanGuard<'_> {
        SpanGuard::start(self, component.into(), category.into(), name.into(), ctx)
    }

    fn append(&self, rec: &YtraceRecord) {
        let Ok(mut line) = serde_json::to_vec(rec) else {
            return;
        };
        line.push(b'\n');
        let path = self.home.join("ytrace.jsonl");
        let _ = fs::create_dir_all(&self.home);
        rotate_if_needed(&path, self.retention, line.len() as u64);
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(&line);
        }
        // also best-effort registry heartbeat
        registry::heartbeat(&self.app, &self.app_version, &self.home, None);
    }

    /// Direct append for tests / compat shim.
    pub fn append_record(&self, rec: &YtraceRecord) {
        self.append(rec);
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
}

fn default_home(app: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("YTRACE_HOME") {
        return PathBuf::from(dir).join(app);
    }
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

// ── span guard ──────────────────────────────────────────────────────────────

pub struct SpanGuard<'a> {
    provider: &'a Provider,
    component: String,
    probe: Probe,
    start: Instant,
    ctx: Value,
    finished: bool,
}

impl<'a> SpanGuard<'a> {
    fn start(
        provider: &'a Provider,
        component: String,
        category: String,
        name: String,
        ctx: Value,
    ) -> Self {
        let probe = provider.resolve_probe(&category, &name);
        Self {
            provider,
            component,
            probe,
            start: Instant::now(),
            ctx,
            finished: false,
        }
    }

    pub fn finish(mut self, payload: Value) {
        self.finished = true;
        if !is_enabled() {
            return;
        }
        let duration_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        if !should_record(&self.probe, duration_ms) {
            return;
        }
        let merged = match (&self.ctx, &payload) {
            (Value::Null, _) => payload,
            (_, Value::Null) => self.ctx.clone(),
            (Value::Object(a), Value::Object(b)) => {
                let mut m = a.clone();
                for (k, v) in b {
                    m.insert(k.clone(), v.clone());
                }
                Value::Object(m)
            }
            _ => payload,
        };
        let rec = YtraceRecord {
            v: YTRACE_WIRE_VERSION,
            ts_ms: now_ms(),
            pid: std::process::id(),
            app: self.provider.app.clone(),
            app_version: self.provider.app_version.clone(),
            component: self.component.clone(),
            category: self.probe.category.clone(),
            name: self.probe.name.clone(),
            clock: self.probe.clock.as_str().to_string(),
            duration_ms: Some(duration_ms),
            payload: merged,
        };
        self.provider.append(&rec);
    }

    /// Annotate without finishing.
    pub fn annotate(&mut self, extra: Value) {
        if let (Value::Object(a), Value::Object(b)) = (&mut self.ctx, &extra) {
            for (k, v) in b {
                a.insert(k.clone(), v.clone());
            }
        } else if self.ctx.is_null() {
            self.ctx = extra;
        }
    }
}

impl<'a> Drop for SpanGuard<'a> {
    fn drop(&mut self) {
        if self.finished || !is_enabled() {
            return;
        }
        let duration_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        if !should_record(&self.probe, duration_ms) {
            return;
        }
        let rec = YtraceRecord {
            v: YTRACE_WIRE_VERSION,
            ts_ms: now_ms(),
            pid: std::process::id(),
            app: self.provider.app.clone(),
            app_version: self.provider.app_version.clone(),
            component: self.component.clone(),
            category: self.probe.category.clone(),
            name: self.probe.name.clone(),
            clock: self.probe.clock.as_str().to_string(),
            duration_ms: Some(duration_ms),
            payload: self.ctx.clone(),
        };
        self.provider.append(&rec);
    }
}

// ── submodules ──────────────────────────────────────────────────────────────

pub mod registry;
pub mod retention_compat;
pub mod query;
pub mod compat;
