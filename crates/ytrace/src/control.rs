//! The ytrace control plane — a per-process Unix socket where script clauses
//! attach, detach, and drain. This is the spec §5.2 socket, activated.
//!
//! Protocol: line-delimited JSON, one request → one response.
//! `{"verb":"attach","id":"slow-frames","script":"render/gui where duration_ms > 16 -> @quantize(duration_ms)"}`
//! → `{"ok":true,"id":"slow-frames","replaced":false}`
//!
//! Semantics: **attach is durable.** The CLI attaches and exits; the script
//! keeps accumulating until an explicit detach or process death. That is the
//! point — always-on instrumentation you attach once and never remove.
//!
//! Locking shape (chosen, not accidental): the map is a read-mostly RwLock; each
//! script state sits in its own Mutex, so the emit path pays one read lock plus
//! one uncontended mutex per firing, and `drain --reset` is atomic against
//! concurrent emitters by taking the same state lock — a rate view cannot
//! double-count or lose events to a racing emit.

use crate::script::{parse, RecRef, ScriptState};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub const PROTOCOL_V: u8 = 1;

type ScriptMap = HashMap<String, Mutex<ScriptState>>;

/// The in-process script engine. Owned by the Provider; the socket server and
/// the emit path both talk to it.
///
/// ⛔ **One per (app, pid) — never one per Provider.** A process may build many
/// Providers (yggterm builds four: narrative trace, perf, row governor, host
/// panic); if each constructed its own engine and socket, every `serve` would
/// unlink the previous listener and strand its attached scripts on an
/// unreachable inode, while the registry's single (app,pid) row flipped its
/// probe catalogue to whichever engine emitted last. `acquire` is the only
/// constructor the Provider path may use: the first Provider for an (app,pid)
/// binds the socket, every later one joins the same engine by `Arc`.
pub struct Control {
    pub app: String,
    scripts: RwLock<ScriptMap>,
    /// Fast gate for the emit path: one relaxed load when no script is attached.
    active: AtomicBool,
    /// Immutable identity of this engine, minted once at creation. Advertised
    /// in `ping`/`catalogue`/`attach` and in the registry row, so a client can
    /// REFUSE when the socket it reached is not the process the registry
    /// described — the shape that drained confident false zeroes.
    gen: u64,
    /// The process-wide probe catalogue: the UNION of every Provider's
    /// registered probes. The registry heartbeat advertises this union, so the
    /// (app,pid) row stops flip-flopping between partial catalogues.
    probes: Mutex<BTreeSet<String>>,
}

/// 64-bit FNV-1a over the sorted catalogue, newline-joined. Deterministic
/// across processes: a client may compare a registry row's digest against a
/// live socket's answer without sharing code with the provider.
fn catalogue_digest_of(probes: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for p in probes {
        feed(p.as_bytes());
        feed(b"\n");
    }
    hash
}

fn mint_gen() -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let addr = &nanos as *const u64 as u64;
    nanos ^ addr.rotate_left(17) ^ ((std::process::id() as u64) << 32)
}

impl Control {
    pub fn new(app: &str) -> Self {
        Control {
            app: app.to_string(),
            scripts: RwLock::new(HashMap::new()),
            active: AtomicBool::new(false),
            gen: mint_gen(),
            probes: Mutex::new(BTreeSet::new()),
        }
    }

    /// Immutable engine identity — stable for the life of the process.
    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// Merge probes into the shared catalogue (idempotent; the union only
    /// grows, like the probes themselves).
    pub fn advertise(&self, probes: &[String]) {
        let mut set = self.probes.lock().unwrap();
        for p in probes {
            set.insert(p.clone());
        }
    }

    /// The live catalogue — every probe registered by any Provider of this
    /// (app, pid), sorted.
    pub fn catalogue(&self) -> Vec<String> {
        self.probes.lock().unwrap().iter().cloned().collect()
    }

    pub fn catalogue_digest(&self) -> u64 {
        catalogue_digest_of(&self.catalogue())
    }

    pub fn has_scripts(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Attach (compile + install). Re-attaching an id replaces it — fresh
    /// aggregates, new clause. Idempotent by id, safe to re-run. An absent id
    /// is derived from the probe + first aggregate.
    pub fn attach(&self, id: Option<String>, clause: &str) -> Result<Value, String> {
        let script = parse(clause, id)?;
        let id = script.id.clone();
        let mut map = self.scripts.write().unwrap();
        let replaced = map.remove(&id).is_some();
        map.insert(id.clone(), Mutex::new(ScriptState::new(script)));
        self.active.store(true, Ordering::Relaxed);
        Ok(json!({"ok": true, "id": id, "replaced": replaced, "v": PROTOCOL_V}))
    }

    pub fn detach(&self, id: &str) -> Value {
        let mut map = self.scripts.write().unwrap();
        let existed = map.remove(id).is_some();
        if map.is_empty() {
            self.active.store(false, Ordering::Relaxed);
        }
        json!({"ok": true, "existed": existed, "v": PROTOCOL_V})
    }

    /// The emit-path hook: every probe firing, unsampled, before the file
    /// stream's sampling gate. Allocation happens only after a predicate match.
    pub fn eval(&self, category: &str, name: &str, r: &RecRef, payload: &Value) {
        let map = self.scripts.read().unwrap();
        for st in map.values() {
            let mut inner = st.lock().unwrap();
            let s = &inner.script;
            if s.category == category && s.name == name {
                inner.eval(r, payload);
            }
        }
    }

    /// Status view: ids, clauses, and the anti-false-zero counters. No bodies.
    pub fn list(&self) -> Value {
        let map = self.scripts.read().unwrap();
        let mut scripts: Vec<Value> = map
            .values()
            .map(|st| {
                let mut s = st.lock().unwrap().drain();
                if let Value::Object(o) = &mut s {
                    o.remove("ring");
                    o.remove("groups");
                }
                s
            })
            .collect();
        scripts.sort_by_key(|s| s["id"].as_str().unwrap_or("").to_string());
        json!({"ok": true, "scripts": scripts, "v": PROTOCOL_V})
    }

    /// Snapshot without reset — read lock only.
    pub fn drain(&self, id: &str) -> Value {
        let map = self.scripts.read().unwrap();
        match map.get(id) {
            Some(st) => {
                let snap = st.lock().unwrap().drain();
                json!({"ok": true, "snapshot": snap, "v": PROTOCOL_V})
            }
            None => json!({"ok": false, "error": format!("no such script `{id}`"), "v": PROTOCOL_V}),
        }
    }

    /// Snapshot + zero, atomic against concurrent emitters (same state lock).
    pub fn drain_reset(&self, id: &str) -> Value {
        let map = self.scripts.read().unwrap();
        match map.get(id) {
            Some(st) => {
                let mut st = st.lock().unwrap();
                let snap = st.drain();
                st.reset();
                json!({"ok": true, "snapshot": snap, "v": PROTOCOL_V})
            }
            None => json!({"ok": false, "error": format!("no such script `{id}`"), "v": PROTOCOL_V}),
        }
    }
}

/// Where control sockets live. `$XDG_RUNTIME_DIR/ytrace` (tmpfs, correct for
/// runtime state); falls back to the data home when RUNTIME_DIR is absent.
pub fn control_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(dir).join("ytrace");
        let _ = std::fs::create_dir_all(&p);
        return Some(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".local/share/ytrace");
        let _ = std::fs::create_dir_all(&p);
        return Some(p);
    }
    None
}

pub fn socket_path(dir: &Path, app: &str, pid: u32) -> PathBuf {
    dir.join(format!("{app}-{pid}.sock"))
}

/// Remove socket files in `dir` that nothing is listening on.
///
/// Every short-lived process that embeds a provider (each CLI invocation)
/// binds a control socket and leaks the file on exit — `$XDG_RUNTIME_DIR` is
/// tmpfs, so dead sockets are resident memory accumulating one per invocation
/// (measured: 10 dead of 14 within minutes of the script plane shipping).
/// Pruning runs at provider start: a file that refuses connections and is
/// older than the grace window is unlinked. The mtime guard protects a
/// process that has bound but not yet connected-verified its own socket.
pub fn prune_dead_sockets(dir: &Path, keep: &Path) {
    prune_dead_sockets_in(dir, keep, 60_000)
}

/// The mechanism, with the grace window explicit for tests.
pub fn prune_dead_sockets_in(dir: &Path, keep: &Path, grace_ms: u128) {
    let now = now_ms_fallback();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p == *keep || p.extension().map(|x| x != "sock").unwrap_or(true) {
            continue;
        }
        if let Ok(meta) = e.metadata() {
            use std::time::UNIX_EPOCH;
            let age = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if now.saturating_sub(age) < grace_ms {
                continue;
            }
            if UnixStream::connect(&p).is_err() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

fn now_ms_fallback() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

/// Bind the control socket and spawn the accept loop. Best-effort: a failure
/// to bind (stale socket from a dead process, another provider same app+pid in
/// a test) leaves the provider fully functional minus remote attach.
///
/// ⛔ Prefer `acquire` on the Provider path — calling `serve` directly per
/// Provider is the defect that orphaned live listeners: the unconditional
/// `remove_file` below is only safe because `acquire` guarantees at most ONE
/// `serve` per (app, pid) per process, so the only thing it can unlink is a
/// genuinely dead socket.
pub fn serve(control: Arc<Control>, dir: &Path, app: &str, pid: u32) -> Option<PathBuf> {
    prune_dead_sockets(dir, &socket_path(dir, app, pid));
    let path = socket_path(dir, app, pid);
    let _ = std::fs::remove_file(&path); // a dead socket file is not an error
    let listener = UnixListener::bind(&path).ok()?;
    let path_out = path.clone();
    std::thread::Builder::new()
        .name(format!("ytrace-ctl-{app}"))
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let control = Arc::clone(&control);
                std::thread::spawn(move || handle_conn(stream, control));
            }
        })
        .ok()?;
    Some(path_out)
}

/// The (app, pid) → shared engine table. The FIRST Provider of a process to
/// call `acquire` creates the engine and binds the socket; every later one
/// joins the same engine and inherits the same socket path. This is what makes
/// `Provider` composable: N providers, one control plane, one registry row,
/// one catalogue (the union).
static SHARED_CONTROLS: Mutex<Option<HashMap<(String, u32), SharedEntry>>> = Mutex::new(None);

struct SharedEntry {
    control: Arc<Control>,
    socket: Option<PathBuf>,
}

/// Acquire the process-shared control plane for `(app, pid)`.
///
/// Returns the engine (shared by every Provider of this process) and the bound
/// socket path (Some only for the engine that won the bind, but the SAME path
/// is returned to joiners — the socket is one per process either way).
pub fn acquire(app: &str, dir: &Path, pid: u32) -> (Arc<Control>, Option<PathBuf>) {
    let mut guard = SHARED_CONTROLS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(entry) = map.get(&(app.to_string(), pid)) {
        return (Arc::clone(&entry.control), entry.socket.clone());
    }
    let control = Arc::new(Control::new(app));
    let socket = serve(Arc::clone(&control), dir, app, pid);
    map.insert(
        (app.to_string(), pid),
        SharedEntry {
            control: Arc::clone(&control),
            socket: socket.clone(),
        },
    );
    (control, socket)
}

/// The attach-time identity check — the known-event canary on the client side.
///
/// gpt-tracing audit 2026-08-30: an attach that reaches the WRONG provider of a
/// multi-provider process is accepted and then drains `fired=0, matched=0,
/// schema_miss=0` — a confident false zero. This check refuses before the
/// script is installed:
///
/// * **generation mismatch** — the registry row named gen A, the live socket
///   answers gen B: the socket is not the process the registry described.
/// * **probe absent** — the clause targets `c/n` and the live provider's
///   catalogue does not contain it: the script would drain zero forever.
///   (`ytrace query` still works — the file plane is a separate positive
///   control.)
///
/// An older provider that answers `ping` without identity and rejects
/// `catalogue` as an unknown verb is tolerated with
/// `catalogue_checked: false` — mixed versions mid-roll must not hard-fail.
#[derive(Debug)]
pub struct CanaryReport {
    pub gen: Option<u64>,
    pub digest: Option<u64>,
    pub probes_n: Option<usize>,
    pub catalogue_checked: bool,
}

pub fn canary(
    sock: &Path,
    expect_gen: Option<u64>,
    probe: Option<(&str, &str)>,
) -> Result<CanaryReport, String> {
    let pong = request(sock, &json!({"verb": "ping"}))?;
    let gen = pong.get("gen").and_then(|v| v.as_u64());
    if let (Some(expect), Some(actual)) = (expect_gen, gen) {
        if expect != actual {
            return Err(format!(
                "socket generation mismatch: the registry row says gen {expect}, the live socket answers gen {actual} — the registry is stale or the socket was rebound by another provider. Re-read `ytrace registry` and attach again; refusing rather than draining a confident false zero."
            ));
        }
    }
    let mut report = CanaryReport {
        gen,
        digest: pong.get("digest").and_then(|v| v.as_u64()),
        probes_n: pong.get("probes_n").and_then(|v| v.as_u64().map(|n| n as usize)),
        catalogue_checked: false,
    };
    if let Ok(cat) = request(sock, &json!({"verb": "catalogue"})) {
        if cat.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            report.catalogue_checked = true;
            report.gen = cat.get("gen").and_then(|v| v.as_u64());
            report.digest = cat.get("digest").and_then(|v| v.as_u64());
            let probes: Vec<String> = cat
                .get("probes")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if let Some((c, n)) = probe {
                let target = format!("{c}/{n}");
                if !probes.iter().any(|p| p == &target) {
                    return Err(format!(
                        "probe `{target}` is not in the live provider's catalogue ({} probes) — the script would drain fired=0 forever. The file plane is a separate positive control: `ytrace query --category {c}` reads records regardless. Refusing the attach.",
                        probes.len()
                    ));
                }
            }
        }
    }
    Ok(report)
}

fn handle_conn(stream: UnixStream, control: Arc<Control>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let (reader, mut writer) = (stream.try_clone().expect("clone"), stream);
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(req) => route(&control, &req),
            Err(e) => json!({"ok": false, "error": format!("bad request: {e}"), "v": PROTOCOL_V}),
        };
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

fn route(control: &Control, req: &Value) -> Value {
    match req.get("verb").and_then(|v| v.as_str()) {
        Some("ping") => {
            let probes = control.catalogue();
            json!({
                "ok": true, "v": PROTOCOL_V, "app": control.app,
                "gen": control.gen(), "digest": catalogue_digest_of(&probes),
                "probes_n": probes.len(),
            })
        }
        Some("catalogue") => {
            let probes = control.catalogue();
            json!({
                "ok": true, "v": PROTOCOL_V,
                "gen": control.gen(), "digest": catalogue_digest_of(&probes),
                "probes": probes,
            })
        }
        Some("attach") => {
            let script = req.get("script").and_then(|v| v.as_str());
            let id = req.get("id").and_then(|v| v.as_str()).map(String::from);
            match script {
                Some(script) => match control.attach(id, script) {
                    Ok(mut resp) => {
                        let probes = control.catalogue();
                        if let Value::Object(map) = &mut resp {
                            map.insert("gen".into(), json!(control.gen()));
                            map.insert("digest".into(), json!(catalogue_digest_of(&probes)));
                            map.insert("probes_n".into(), json!(probes.len()));
                        }
                        resp
                    }
                    Err(e) => json!({"ok": false, "error": e, "v": PROTOCOL_V}),
                },
                None => json!({"ok": false, "error": "attach needs `script` (and optionally `id`)", "v": PROTOCOL_V}),
            }
        }
        Some("detach") => match req.get("id").and_then(|v| v.as_str()) {
            Some(id) => control.detach(id),
            None => json!({"ok": false, "error": "detach needs `id`", "v": PROTOCOL_V}),
        },
        Some("scripts") => control.list(),
        Some("drain") => {
            let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let reset = req.get("reset").and_then(|v| v.as_bool()).unwrap_or(false);
            if reset {
                control.drain_reset(id)
            } else {
                control.drain(id)
            }
        }
        Some(other) => json!({"ok": false, "error": format!("unknown verb `{other}` (verbs: ping catalogue attach detach scripts drain)"), "v": PROTOCOL_V}),
        None => json!({"ok": false, "error": "request needs a `verb`", "v": PROTOCOL_V}),
    }
}

/// One request → one response, from a client (the CLI, tests, a notebook).
pub fn request(sock: &Path, req: &Value) -> Result<Value, String> {
    let mut stream =
        UnixStream::connect(sock).map_err(|e| format!("connect {}: {e}", sock.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).map_err(|e| e.to_string())?;
    serde_json::from_str(&resp).map_err(|e| format!("bad response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec() -> (RecRef<'static>, Value) {
        (
            RecRef {
                ts_ms: 1,
                pid: 1,
                app: "t",
                app_version: "0",
                component: "ui",
                category: "render",
                name: "gui",
                clock: "cpu",
                duration_ms: Some(20.0),
            },
            json!({"rows": 10}),
        )
    }

    #[test]
    fn attach_detach_drain_lifecycle() {
        let c = Control::new("t");
        let r = c
            .attach(
                Some("slow".into()),
                "render/gui where duration_ms > 16 -> @count",
            )
            .expect("compiles");
        assert_eq!(r["ok"], true);
        assert_eq!(r["replaced"], false);

        let (r, payload) = rec();
        c.eval("render", "gui", &r, &payload);
        c.eval("render", "gui", &r, &payload);
        c.eval("other", "probe", &r, &payload); // must not count

        let d = c.drain("slow");
        assert_eq!(d["snapshot"]["stats"]["matched"], 2);

        // idempotent re-attach resets aggregates
        c.attach(Some("slow".into()), "render/gui -> @count").unwrap();
        let d = c.drain("slow");
        assert_eq!(d["snapshot"]["stats"]["fired"], 0, "re-attach = fresh aggregates");

        assert_eq!(c.detach("slow")["existed"], true);
        assert_eq!(c.detach("slow")["existed"], false);
        assert_eq!(c.drain("slow")["ok"], false);
    }

    #[test]
    fn drain_reset_is_a_rate_view() {
        let c = Control::new("t");
        c.attach(Some("all".into()), "render/gui -> @count").unwrap();
        let (r, payload) = rec();
        c.eval("render", "gui", &r, &payload);
        let a = c.drain_reset("all");
        assert_eq!(a["snapshot"]["stats"]["matched"], 1);
        let b = c.drain("all");
        assert_eq!(b["snapshot"]["stats"]["matched"], 0, "reset drained the counters");
        c.eval("render", "gui", &r, &payload);
        let d = c.drain_reset("all");
        assert_eq!(d["snapshot"]["stats"]["matched"], 1);
    }

    #[test]
    fn prune_dead_sockets_removes_only_unconnectable_old_files() {
        let dir = std::env::temp_dir().join(format!("ytrace-prune-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let control = Arc::new(Control::new("prune-app"));
        let live = serve(Arc::clone(&control), &dir, "prune-app", 888_001).expect("binds");
        // a dead socket: bind a listener, drop it, leave the file. (Opening
        // the file ENXIOs once the listener is gone — that is the definition
        // of dead here.)
        let dead_path = dir.join("dead-999999.sock");
        {
            let l = UnixListener::bind(&dead_path).unwrap();
            drop(l);
        }
        // a fresh socket file inside the grace window: never touched
        let fresh_path = dir.join("fresh-999998.sock");
        {
            let l = UnixListener::bind(&fresh_path).unwrap();
            drop(l);
        }

        prune_dead_sockets_in(&dir, &live, 0);

        assert!(live.exists(), "the live socket survives pruning");
        assert!(!dead_path.exists(), "the dead socket file is reaped");
        assert!(!fresh_path.exists(), "grace 0 reaps everything dead, fresh included");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_respects_the_grace_window() {
        let dir = std::env::temp_dir().join(format!("ytrace-prune2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let dead_path = dir.join("dead-999997.sock");
        {
            let l = UnixListener::bind(&dead_path).unwrap();
            drop(l);
        }
        // huge grace: nothing dies, the starting process is never raced
        prune_dead_sockets_in(&dir, &dir.join("self.sock"), u128::MAX);
        assert!(dead_path.exists(), "a fresh dead file inside the grace window survives");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wire_roundtrip_ping_attach_drain() {
        let dir = std::env::temp_dir().join(format!("ytrace-ctl-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let control = Arc::new(Control::new("wireapp"));
        let path = serve(Arc::clone(&control), &dir, "wireapp", 777_001).expect("binds");
        assert!(path.exists());

        let pong = request(&path, &json!({"verb": "ping"})).unwrap();
        assert_eq!(pong["ok"], true);

        let att = request(
            &path,
            &json!({"verb": "attach", "id": "s1", "script": "render/gui where duration_ms > 16 -> @quantize(duration_ms)"}),
        )
        .unwrap();
        assert_eq!(att["ok"], true, "{att}");

        let (r, payload) = rec();
        control.eval("render", "gui", &r, &payload);

        let d = request(&path, &json!({"verb": "drain", "id": "s1"})).unwrap();
        assert_eq!(d["snapshot"]["stats"]["matched"], 1);

        let det = request(&path, &json!({"verb": "detach", "id": "s1"})).unwrap();
        assert_eq!(det["existed"], true);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_errors_come_back_over_the_wire() {
        let dir = std::env::temp_dir().join(format!("ytrace-ctl-test2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let control = Arc::new(Control::new("wireapp2"));
        let path = serve(Arc::clone(&control), &dir, "wireapp2", 777_002).expect("binds");
        let att = request(
            &path,
            &json!({"verb": "attach", "id": "bad", "script": "render/gui -> @nosuch"}),
        )
        .unwrap();
        assert_eq!(att["ok"], false);
        assert!(att["error"].as_str().unwrap().contains("unknown aggregate"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── the multi-provider fix (gpt-tracing audit, 2026-08-30) ─────────────

    /// THE audit reproduction, as a regression lock: two Providers of one
    /// (app, pid) used to mean the second `serve` unlinked the first's live
    /// socket — its listener stranded on an unreachable inode, its scripts
    /// unreachable, the registry catalogue flipped. `acquire` makes the second
    /// provider JOIN the first's engine instead.
    #[test]
    fn second_acquire_joins_the_first_engine_instead_of_rebinding() {
        let dir = std::env::temp_dir().join(format!("ytrace-acquire-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let (c1, s1) = acquire("acquire-join-app", &dir, 424_242);
        let (c2, s2) = acquire("acquire-join-app", &dir, 424_242);

        assert!(
            Arc::ptr_eq(&c1, &c2),
            "both providers must share ONE engine, not two"
        );
        assert_eq!(s1, s2, "the socket path must not be rebound by the joiner");
        assert!(s1.as_ref().is_some_and(|p| p.exists()), "the socket stays live");

        // A script attached through the first engine sees events from the
        // second provider — one control plane, composable providers.
        c1.attach(Some("audit".into()), "daemon_request/snapshot -> @count")
            .unwrap();
        c2.eval(
            "daemon_request",
            "snapshot",
            &RecRef {
                ts_ms: 1,
                pid: 1,
                app: "acquire-join-app",
                app_version: "0",
                component: "d",
                category: "daemon_request",
                name: "snapshot",
                clock: "wall",
                duration_ms: None,
            },
            &json!({}),
        );
        let d = c1.drain("audit");
        assert_eq!(
            d["snapshot"]["stats"]["matched"], 1,
            "the shared engine routes every provider's emissions"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ping_and_catalogue_carry_identity_and_the_union() {
        let dir = std::env::temp_dir().join(format!("ytrace-ident-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let control = Arc::new(Control::new("ident-app"));
        control.advertise(&["b/second".into()]);
        let path = serve(Arc::clone(&control), &dir, "ident-app", 777_003).expect("binds");
        control.advertise(&["a/first".into()]); // after bind — union still grows

        let pong = request(&path, &json!({"verb": "ping"})).unwrap();
        assert_eq!(pong["ok"], true);
        assert!(pong["gen"].as_u64().is_some_and(|g| g != 0), "gen is minted");
        assert_eq!(pong["probes_n"], 2, "the catalogue is the union");

        let cat = request(&path, &json!({"verb": "catalogue"})).unwrap();
        let probes: Vec<String> = cat["probes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert_eq!(probes, vec!["a/first", "b/second"], "sorted union");
        assert_eq!(cat["digest"], pong["digest"], "digest agrees across verbs");
        assert_eq!(cat["gen"], pong["gen"]);

        // attach echoes identity too — one roundtrip can carry the canary.
        let att = request(
            &path,
            &json!({"verb": "attach", "id": "s9", "script": "a/first -> @count"}),
        )
        .unwrap();
        assert_eq!(att["ok"], true);
        assert_eq!(att["gen"], pong["gen"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_is_order_independent_and_deterministic() {
        let a = Control::new("digest-a");
        a.advertise(&["x/one".into(), "y/two".into()]);
        let b = Control::new("digest-b");
        b.advertise(&["y/two".into(), "x/one".into()]);
        assert_eq!(a.catalogue_digest(), b.catalogue_digest());
        b.advertise(&["z/three".into()]);
        assert_ne!(a.catalogue_digest(), b.catalogue_digest());
    }

    #[test]
    fn canary_refuses_a_generation_mismatch() {
        let dir = std::env::temp_dir().join(format!("ytrace-gen-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let control = Arc::new(Control::new("gen-app"));
        let path = serve(Arc::clone(&control), &dir, "gen-app", 777_004).expect("binds");
        let wrong = control.gen() ^ 0xffff;
        let err = canary(&path, Some(wrong), None).expect_err("gen mismatch must refuse");
        assert!(err.contains("generation mismatch"), "{err}");
        // the CORRECT gen passes
        canary(&path, Some(control.gen()), None).expect("matching gen passes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn canary_refuses_a_probe_outside_the_live_catalogue() {
        let dir = std::env::temp_dir().join(format!("ytrace-cat-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let control = Arc::new(Control::new("cat-app"));
        control.advertise(&["trace/gui".into()]);
        let path = serve(Arc::clone(&control), &dir, "cat-app", 777_005).expect("binds");

        let err = canary(&path, None, Some(("daemon_request", "snapshot")))
            .expect_err("absent probe must refuse");
        assert!(err.contains("daemon_request/snapshot"), "{err}");
        assert!(err.contains("catalogue"), "{err}");

        let ok = canary(&path, None, Some(("trace", "gui"))).expect("present probe passes");
        assert!(ok.catalogue_checked);
        assert_eq!(ok.probes_n, Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An older provider answers `ping` without identity and rejects
    /// `catalogue` as an unknown verb. Mixed versions mid-roll must degrade to
    /// "unverified", never hard-fail.
    #[test]
    fn canary_tolerates_an_old_provider_without_identity() {
        let dir = std::env::temp_dir().join(format!("ytrace-old-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("old-provider.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            // exactly the two requests a canary makes: ping, then catalogue
            for _ in 0..2 {
                let Some(stream) = listener.incoming().flatten().next() else {
                    break;
                };
                let mut writer = stream.try_clone().unwrap();
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).is_err() {
                    break;
                }
                let resp = if line.contains("\"ping\"") {
                    json!({"ok": true, "v": 1, "app": "old"})
                } else {
                    json!({"ok": false, "error": "unknown verb (verbs: ping attach detach scripts drain)", "v": 1})
                };
                let mut out = serde_json::to_string(&resp).unwrap();
                out.push('\n');
                let _ = writer.write_all(out.as_bytes());
            }
        });
        let report = canary(&path, Some(12345), Some(("render", "gui")))
            .expect("an old provider degrades to unverified, not refusal");
        assert!(!report.catalogue_checked);
        assert_eq!(report.gen, None);
        server.join().ok();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
