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
use std::collections::HashMap;
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
pub struct Control {
    pub app: String,
    scripts: RwLock<ScriptMap>,
    /// Fast gate for the emit path: one relaxed load when no script is attached.
    active: AtomicBool,
}

impl Control {
    pub fn new(app: &str) -> Self {
        Control {
            app: app.to_string(),
            scripts: RwLock::new(HashMap::new()),
            active: AtomicBool::new(false),
        }
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

/// Bind the control socket and spawn the accept loop. Best-effort: a failure
/// to bind (stale socket from a dead process, another provider same app+pid in
/// a test) leaves the provider fully functional minus remote attach.
pub fn serve(control: Arc<Control>, dir: &Path, app: &str, pid: u32) -> Option<PathBuf> {
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
        Some("ping") => json!({"ok": true, "v": PROTOCOL_V, "app": control.app}),
        Some("attach") => {
            let script = req.get("script").and_then(|v| v.as_str());
            let id = req.get("id").and_then(|v| v.as_str()).map(String::from);
            match script {
                Some(script) => match control.attach(id, script) {
                    Ok(resp) => resp,
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
        Some(other) => json!({"ok": false, "error": format!("unknown verb `{other}` (verbs: ping attach detach scripts drain)"), "v": PROTOCOL_V}),
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
}
