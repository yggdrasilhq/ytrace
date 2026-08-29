use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
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
    generations_max_bytes: 1024 * 1024 * 1024,
    max_age_ms: DEFAULT_MAX_AGE_MS,
};

fn is_dev_mode() -> bool {
    if std::env::var("YGGTERM_DEV")
        .map(|v| v == "1" || v.to_ascii_lowercase() == "true")
        .unwrap_or(false)
    {
        return true;
    }
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".yggterm/config/dev-mode");
        if let Ok(c) = std::fs::read_to_string(path) {
            let trimmed = c.trim().to_ascii_lowercase();
            if trimmed == "1" || trimmed == "true" || trimmed == "yes" {
                return true;
            }
        }
    }
    false
}

fn default_retention() -> Retention {
    let generations_max_bytes = if is_dev_mode() {
        10 * 1024 * 1024 * 1024
    } else {
        1024 * 1024 * 1024
    };
    Retention {
        live_max_bytes: 8 * 1024 * 1024,
        generations_max_bytes,
        max_age_ms: DEFAULT_MAX_AGE_MS,
    }
}

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
    /// Held file handle — the emit path must not pay open/close per record.
    out: Mutex<OutFile>,
    /// The script engine. Always present; `has_scripts()` is the hot-path gate.
    control: Arc<control::Control>,
    /// Advertised in the registry when the control socket bound successfully.
    socket: Option<PathBuf>,
}

/// Byte interval between rotation checks on the held handle. The old path paid
/// a `stat` per record; the file's size only needs sampling at this cadence.
const ROTATION_CHECK_BYTES: u64 = 64 * 1024;

struct OutFile {
    file: Option<File>,
    written_since_check: u64,
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
        let app = app.into();
        // Script engine + best-effort control socket. A failed bind (stale
        // socket, pid collision) costs nothing: emission is unaffected.
        let control = Arc::new(control::Control::new(&app));
        let socket = control::control_dir()
            .and_then(|dir| control::serve(Arc::clone(&control), &dir, &app, std::process::id()));
        Self {
            app,
            app_version: app_version.into(),
            home,
            probes: Mutex::new(HashMap::new()),
            retention: default_retention(),
            out: Mutex::new(OutFile {
                file: None,
                written_since_check: 0,
            }),
            control,
            socket,
        }
    }

    /// The script engine — attach/drain from tests or in-process callers.
    pub fn control(&self) -> &Arc<control::Control> {
        &self.control
    }

    /// The control socket path, when the server bound (advertised in registry).
    pub fn socket_path(&self) -> Option<&Path> {
        self.socket.as_deref()
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

    /// The script-plane hook: every probe firing reaches attached scripts,
    /// unsampled — sampling is a FILE-stream policy, and a `@quantize` that saw
    /// only 1:50 of fast frames would be a lying instrument. Runs before the
    /// sampling gate; no allocation unless a predicate matched.
    fn scripts_eval(
        &self,
        component: &str,
        category: &str,
        name: &str,
        clock: &str,
        duration_ms: Option<f64>,
        payload: &Value,
        ts_ms: u128,
    ) {
        if self.control.has_scripts() {
            let r = script::RecRef {
                ts_ms,
                pid: std::process::id(),
                app: &self.app,
                app_version: &self.app_version,
                component,
                category,
                name,
                clock,
                duration_ms,
            };
            self.control.eval(category, name, &r, payload);
        }
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
        let component = component.into();
        let category = category.into();
        let name = name.into();
        let ts_ms = now_ms();
        self.scripts_eval(
            &component,
            &category,
            &name,
            Clock::Wall.as_str(),
            None,
            &payload,
            ts_ms,
        );
        let rec = YtraceRecord {
            v: YTRACE_WIRE_VERSION,
            ts_ms,
            pid: std::process::id(),
            app: self.app.clone(),
            app_version: self.app_version.clone(),
            component,
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
        let len = line.len() as u64;
        let mut out = self.out.lock().unwrap();
        if out.file.is_none() {
            let _ = fs::create_dir_all(&self.home);
            out.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.home.join("ytrace.jsonl"))
                .ok();
            out.written_since_check = 0;
        }
        if let Some(f) = out.file.as_mut() {
            let _ = f.write_all(&line);
            out.written_since_check += len;
            if out.written_since_check >= ROTATION_CHECK_BYTES {
                out.written_since_check = 0;
                self.rotation_check(&mut out.file, len);
            }
        }
        drop(out);
        // Best-effort registry heartbeat. This is on the EMIT path, so it runs
        // once per record; the 15 s interval gate keeps the discovery index
        // from growing at the rate of the event stream.
        let probes: Vec<String> = self
            .probes
            .lock()
            .map(|m| m.keys().map(|(c, n)| format!("{c}/{n}")).collect())
            .unwrap_or_default();
        registry::heartbeat_with_probes(
            &self.app,
            &self.app_version,
            &self.home,
            self.socket.as_deref(),
            &probes,
        );
    }

    /// Rotation + liveness check on the held handle, run every
    /// `ROTATION_CHECK_BYTES` instead of per record. Inode-aware: if another
    /// process rotated under us, our handle points at a renamed generation —
    /// detect by (dev, ino) identity and reopen the live path, so records keep
    /// landing where readers look.
    fn rotation_check(&self, file: &mut Option<File>, incoming: u64) {
        use std::os::unix::fs::MetadataExt;
        let path = self.home.join("ytrace.jsonl");
        let ours_id = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| (m.dev(), m.ino()));
        let live = fs::metadata(&path).ok();
        let live_id = live.as_ref().map(|m| (m.dev(), m.ino()));
        match (&ours_id, &live_id) {
            (Some(a), Some(b)) if a != b => {
                // rotated elsewhere — adopt the live file
                *file = OpenOptions::new().create(true).append(true).open(&path).ok();
                return;
            }
            (Some(_), None) => {
                // live file vanished (manual prune) — recreate by reopening
                *file = OpenOptions::new().create(true).append(true).open(&path).ok();
                return;
            }
            _ => {}
        }
        let size = live.map(|m| m.len()).unwrap_or(0);
        if size.saturating_add(incoming) > self.retention.live_max_bytes {
            let ts = now_ms();
            let gen = generation_path(&path, ts);
            let _ = fs::rename(&path, &gen);
            prune_generations(&path, self.retention, ts);
            *file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok();
        }
    }

    /// Emit an incident — a ytrace record with payload.incident=true, for
    /// governor faults, LLM complaints, and Dash notebook chapters.
    /// Always recorded (no sampling), because loss of a fault is worse than loss of a span.
    pub fn incident(
        &self,
        component: impl Into<String>,
        category: impl Into<String>,
        name: impl Into<String>,
        payload: Value,
    ) {
        if !is_enabled() {
            return;
        }
        // Ensure incident flag and complaint marker survive merge with any caller payload.
        let mut merged = payload;
        if let Value::Object(ref mut map) = merged {
            map.entry("incident").or_insert(Value::Bool(true));
            map.entry("complaint_for").or_insert(json!("llm"));
        } else if merged.is_null() {
            merged = json!({"incident": true, "complaint_for": "llm"});
        } else {
            // non-object payload: wrap
            merged = json!({"incident": true, "complaint_for": "llm", "data": merged});
        }
        // Scripts watch incidents too (e.g. `governor/fault -> @count`).
        let ts_ms = now_ms();
        let comp = component.into();
        let cat = category.into();
        let nm = name.into();
        self.scripts_eval(&comp, &cat, &nm, Clock::Wall.as_str(), None, &merged, ts_ms);
        let rec = YtraceRecord {
            v: YTRACE_WIRE_VERSION,
            ts_ms,
            pid: std::process::id(),
            app: self.app.clone(),
            app_version: self.app_version.clone(),
            component: comp,
            category: cat,
            name: nm,
            clock: Clock::Wall.as_str().to_string(),
            duration_ms: None,
            payload: merged,
        };
        self.append(&rec);
    }

    /// Convenience: emit a diagnosed incident via `crate::diagnosis::Incident`.
    pub fn incident_from_diagnosis(
        &self,
        component: &str,
        category: &str,
        name: &str,
        incident: &crate::diagnosis::Incident,
    ) {
        self.incident(
            component,
            category,
            name,
            crate::diagnosis::incident_payload(incident),
        );
    }

    /// Emit a metric (point value) — gauge/counter attached as payload.metric.
    pub fn metric(
        &self,
        component: impl Into<String>,
        category: impl Into<String>,
        name: impl Into<String>,
        value: f64,
        unit: &str,
        extra: Value,
    ) {
        if !is_enabled() {
            return;
        }
        let payload = match extra {
            Value::Object(mut m) => {
                m.insert("metric".to_string(), json!({"value": value, "unit": unit}));
                Value::Object(m)
            }
            Value::Null => json!({"metric": {"value": value, "unit": unit}}),
            other => json!({"metric": {"value": value, "unit": unit}, "data": other}),
        };
        self.event(component, category, name, payload);
    }

    /// Direct append for tests / compat shim.
    pub fn append_record(&self, rec: &YtraceRecord) {
        self.append(rec);
    }

    /// Emit a span with explicit duration (for externally-timed probes like yggterm PerfSpan).
    /// Mirrors `Provider::span` but takes `duration_ms` caller-measured, not `Instant::now()` delta.
    pub fn emit_span(
        &self,
        component: impl Into<String>,
        category: impl Into<String>,
        name: impl Into<String>,
        clock: Clock,
        duration_ms: f64,
        payload: Value,
    ) {
        if !is_enabled() {
            return;
        }
        let component_s = component.into();
        let category_s = category.into();
        let name_s = name.into();
        let ts_ms = now_ms();
        // scripts see every span, unsampled, before the file gate
        self.scripts_eval(
            &component_s,
            &category_s,
            &name_s,
            clock.as_str(),
            Some(duration_ms),
            &payload,
            ts_ms,
        );
        if let Some(p) = self.probe_for(&category_s, &name_s) {
            if !should_record(&p, duration_ms) {
                return;
            }
        }
        let rec = YtraceRecord {
            v: YTRACE_WIRE_VERSION,
            ts_ms,
            pid: std::process::id(),
            app: self.app.clone(),
            app_version: self.app_version.clone(),
            component: component_s,
            category: category_s,
            name: name_s,
            clock: clock.as_str().to_string(),
            duration_ms: Some(duration_ms),
            payload,
        };
        self.append(&rec);
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
        let scripts_on = self.provider.control.has_scripts();
        if !scripts_on && !should_record(&self.probe, duration_ms) {
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
        let ts_ms = now_ms();
        self.provider.scripts_eval(
            &self.component,
            &self.probe.category,
            &self.probe.name,
            self.probe.clock.as_str(),
            Some(duration_ms),
            &merged,
            ts_ms,
        );
        if !should_record(&self.probe, duration_ms) {
            return;
        }
        let rec = YtraceRecord {
            v: YTRACE_WIRE_VERSION,
            ts_ms,
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
        let scripts_on = self.provider.control.has_scripts();
        if !scripts_on && !should_record(&self.probe, duration_ms) {
            return;
        }
        if scripts_on {
            let ts_ms = now_ms();
            self.provider.scripts_eval(
                &self.component,
                &self.probe.category,
                &self.probe.name,
                self.probe.clock.as_str(),
                Some(duration_ms),
                &self.ctx,
                ts_ms,
            );
        }
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
pub mod diagnosis;
pub mod script;
pub mod control;
