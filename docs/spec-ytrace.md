# Spec: ytrace — DTrace-like provider interface for the ytop fleet

**Status:** PROPOSED 2026-08-17 · **Owner:** ytrace / ytop / yggterm · **SSOT for the wire**

`ytrace` is the single probe surface that every fleet app (yggterm, ytop, ychrome, yedit, and proprietary webapps) speaks, and `ytop` is the single reader. It replaces app-specific `perf-telemetry.jsonl` / `event-trace.jsonl` / `/proc` walks with one discoverable, versioned, file-first transport.

```
                     ytrace PROVIDERS (any app)
          yggterm ──► ytrace spans/events/metrics/incidents ──┐
            ytop ──►   (append JSONL + live Unix socket)      │  registry
         ychrome ──►                                 ───────┼──► $XDG_RUNTIME_DIR/ytrace/registry.jsonl
   proprietary ──►                                    ──────┘        │
                                                                     ▼
                                                               ytrace CONSUMERS
                                                                 ytop probe fan-out
                                                                 ytop Dash: timeline × row × span
                                                                 ytop Top:  host × process × metric
                                                                 CLI: ytrace query --provider yggterm --category render
```

---

## 1. Concepts (DTrace analogy)

| DTrace | ytrace | Example |
|---|---|---|
| provider | `app` id | `yggterm`, `ychrome`, `ytop`, `myapp` |
| module | `component` | `daemon`, `shell`, `render`, `web_surface` |
| probe | `probe` = `category/name` | `daemon_request/status`, `render/gui`, `web/fetch` |
| predicate | sampling policy | `floor=8ms sample 1:50` |
| action | `clock` + `payload` | `wall 1.38ms {rows:54}` · `cpu 0.12ms {gpu_ms:1.4}` |

* A provider **declares** probes; the wire is append-only JSONL so a reader never needs the declarer live.
* Zero overhead when off: `ytrace::is_enabled()` is one relaxed atomic; `Span`/`Event` constructors early-return. Spans stay compiled in.

---

## 2. Wire format — one record, three kinds

Every record is one JSON line. Common header:

```json
{
  "v": 1,
  "ts_ms": 1723900000123,
  "pid": 3102070,
  "app": "yggterm",
  "app_version": "3.0.154",
  "component": "daemon",
  "category": "daemon_request",
  "name": "status",
  "clock": "wall",
  "duration_ms": 1.385,
  "payload": {"rows": 54, "host": "jojo"}
}
```

| field | type | meaning |
|---|---|---|
| `v` | u8 | wire version (1) |
| `ts_ms` | u128 | `SystemTime::now` millis since epoch — the **only** clock for ordering; `duration_ms` is the span's own clock |
| `pid` | u32 | writer pid — every writer appends to the same HOME, so a record that cannot name its writer cannot be attributed |
| `app` | string | provider id `[a-z0-9_-]{1,32}` lowercase |
| `app_version` | string | semver or git sha — so a consumer can know which fixes are live |
| `component` | string | module inside app |
| `category` | string | probe category |
| `name` | string | probe name |
| `clock` | `"wall"` \| `"cpu"` | which clock `duration_ms` is on (see §3) |
| `duration_ms` | f64 | span latency (spans only; events omit / `null`) |
| `payload` | JSON | probe-specific context |

Kind is inferred: `duration_ms` present → **span**; `metric: {value, unit}` in payload → **metric**; `incident: true` → **incident** (aggregated threshold snapshot). Plain records are **events**.

---

## 3. Clocks — wall vs cpu is a property of the probe kind, not the writer

* `wall`: `Instant::now()` delta — request latency, attach duration, copy scan.
* `cpu`: `CLOCK_THREAD_CPUTIME_ID` / `getrusage(RUSAGE_THREAD)` / `/proc/<pid>/stat` delta divided by elapsed — render `gpu_ms`, `client_handler_cost`, `status_cost`.

`ytrace::probe_clock(category)` is the single owner (like `perf_span_time_base`), so retroactive reclassification works without rewriting history.

---

## 4. Sampling — floor + sample, never silent drop

High-frequency probes (`daemon_request/status`, `ping`, `terminal_read/write`) use:

* `floor = 8.0 ms`: keep every slow outlier (worth seeing even if noisy)
* `sample = 1:50`: keep 2% of the fast rest so the **rate stays visible** (`count × 50`)

All other probes are always recorded. Both parameters are per-probe and overridable at registration; the wire never loses the fact that sampling happened — consumers multiply the count.

---

## 5. Transport — file first, socket second

### 5.1 File (historical, always available)

```
$XDG_DATA_HOME/ytrace/<app>/ytrace.jsonl            # live
$XDG_DATA_HOME/ytrace/<app>/ytrace.g<ts_ms>.jsonl   # generations
~/.yggterm/ytrace/<app>/ytrace.jsonl                # legacy alias for yggterm (compat)
```

* Generational retention (copied from `yggterm-core::retention`): `live_max_bytes` + `generations_max_bytes` + `max_age_ms`. Prune only at rotation + first write per process — one append per event, no scan.
* Budget is **per app HOME while write rate is per process**: window ≈ `budget / (per-process rate × N)`. Size the budget in **bytes at the observed rate**, never in days.

### 5.2 Live socket (optional, for `snapshot`/` tenants` style queries)

```
$XDG_RUNTIME_DIR/ytrace/<app>-<pid>.sock
```

* JSON request/response over Unix socket (`{"verb":"snapshot"}` → `{"v":1, ...}`), versioned. Absence is not an error — file transport already carries the history.

### 5.3 Registry (discovery)

```
$XDG_RUNTIME_DIR/ytrace/registry.jsonl   # one line per provider heartbeat
~/.local/share/ytrace/registry.jsonl      # fallback when RUNTIME_DIR absent
```

Each provider every 15 s appends (or upserts in `registry.json` alternative) :

```json
{"app":"yggterm","pid":3102070,"version":"3.0.154","home":"/home/pi/.local/share/ytrace/yggterm","socket":"/run/user/1000/ytrace/yggterm-3102070.sock","ts_ms":1723900000123,"probes":["daemon_request/status","render/gui"]}
```

* `ytop` discovers by reading the registry (local read or ssh `cat` of the same path) — no installed agent, no argv-smuggling (`ssh host python3 -c` trap), just stdin-fed JSON.
* Stale entries: `now - ts_ms > 45s` → dead provider (same as `ControlMaster 45s` for ytop fan-out).

---

## 6. Query verbs (what ytop actually calls)

`ytop` never parses raw JSONL itself; it calls `ytrace` client verbs which fan-out over ssh with the same stdin-fed script:

* `ytrace query --app yggterm --category daemon_request --name status --since 60s --top 10 --json` — ranked summary (like `server perf-summary`), clock-aware (`cpu` vs `wall`).
* `ytrace query --app yggterm --category render --since 15s --json` — per-role `cores`/`gpu_ms` (like `render_probe`).
* `ytrace tail --app yggterm --category trace --since 200` — last N events (like `server trace tail`).
* `ytrace incidents --app yggterm --since 1h` — `perf-incidents` durable snapshots.
* `ytrace tenants --app yggterm` — live row tenant probe (still via daemon; wrapped as ytrace `tenants` probe for uniform discovery).

Local only, in-process (`~/.local/share/ytrace/...`) so they answer even when the daemon is busy — same guarantee as `yggterm-headless server perf-summary`.

---

## 7. Integration contract — 5 lines to become a provider

```rust
// Cargo.toml
ytrace = { path = "../ytrace" }

use ytrace::{Provider, probe};

// once at startup
static YT: Provider = Provider::new("myapp", env!("CARGO_PKG_VERSION"));
YT.register_probe("http/fetch", ytrace::Clock::Wall, ytrace::Sample::always());

// per request
let _span = YT.span("http/fetch", json!({"url": url}));
// ... work ...
_span.finish(json!({"status": 200, "bytes": n}));

// point event
YT.event("cache/evict", json!({"keys": 3}));
```

* `Provider::new` writes the registry entry and opens the file handle.
* `is_enabled()` check is free when `YTRACE_ENABLED=0` or `settings.perf_profiling_enabled == false`.
* Proprietary apps set `YTRACE_HOME=/var/log/ytrace/myapp` — nothing yggterm-specific leaks in.

---

## 8. Versioning & migration

* Wire `v:1` is frozen; new optional fields are additive.
* `yggterm`'s existing `perf-telemetry.jsonl` / `event-trace.jsonl` become **ytrace file aliases** — `ytrace::compat::yggterm_home()` returns the existing path so old and new readers share the same bytes during migration. No disk migration.

---

## 9. Verification (spec is done when)

* `cargo test -p ytrace` — retention, sampling, registry, query verbs.
* `ytop --once --mode dash --json` shows `yggterm` as a ytrace provider with live `status` `1.38ms` and `render` `0.01 cores` sourced from ytrace, not ad-hoc `/proc` walk.
* `yggterm-headless server perf-summary` and `ytrace query --app yggterm --category render` agree within `1.4%` (the `4.65 µs/row` slope).

---

## 10. File map

```
~/gh/ytrace/
  docs/spec-ytrace.md              # this file
  crates/ytrace/src/
    lib.rs                         # Provider, Probe, Span, Event, Metric
    retention.rs                   # generational retention (from yggterm-core)
    registry.rs                    # discovery heartbeat + prune
    query.rs                       # summarize / tail / incidents client
    compat.rs                      # yggterm/perf alias shim
  .agents/skills/ytrace/SKILL.md   # agent dev integration guide (in ytop + ytrace)
```
