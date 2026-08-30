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
| predicate | sampling policy, or a runtime script `where` clause (§11) | `floor=8ms sample 1:50` · `where duration_ms > 16` |
| action | `clock` + `payload`; script aggregates `@quantize/@count/...` (§11) | `wall 1.38ms {rows:54}` · `@quantize(duration_ms) by payload.host_id` |

* A provider **declares** probes; the wire is append-only JSONL so a reader never needs the declarer live.
* Zero overhead when off: `ytrace::is_enabled()` is one relaxed atomic; `Span`/`Event` constructors early-return. Spans stay compiled in.
* Scripts (§11) attach at runtime over the control socket; the hot path pays one relaxed atomic load when none are attached.

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
  "payload": {"rows": 54, "host": "example-host"}
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
* **One socket per (app, pid) — never one per Provider.** A process may build
  many Providers; they all join one control plane via `control::acquire`. The
  first binds; joiners share the engine by `Arc`. A Provider that called
  `serve` directly would unlink the previous live listener (its scripts
  stranded on an unreachable inode) — the defect the 2026-08-30
  multi-provider audit caught live.
* **Identity.** Every control-plane answer (`ping`, `catalogue`, `attach`)
  carries `gen` (immutable per process, minted at bind) and `digest` (FNV-1a
  of the sorted catalogue). The registry row carries the same `gen` as
  `socket_gen`; an attaching client REFUSES on mismatch instead of installing
  a script that would drain a confident false zero.

### 5.3 Registry (discovery)

```
$XDG_RUNTIME_DIR/ytrace/registry.jsonl   # one line per provider heartbeat
~/.local/share/ytrace/registry.jsonl      # fallback when RUNTIME_DIR absent
```

Each provider every 15 s appends (or upserts in `registry.json` alternative) :

```json
{"app":"yggterm","pid":3102070,"version":"3.0.154","home":"/home/user/.local/share/ytrace/yggterm","socket":"/run/user/1000/ytrace/yggterm-3102070.sock","ts_ms":1723900000123,"probes":["daemon_request/status","render/gui"],"socket_gen":16647493043968770284,"catalogue_digest":14732332207944016278}
```

* `probes` is the **process-wide UNION** of every Provider's registered probes
  (from the shared engine's catalogue) — never one provider's slice. The
  per-provider slice let whichever provider emitted first after the heartbeat
  gate claim the row with a partial catalogue; the advertised probes
  flip-flopped between 3 and 33 with no restart (live-witnessed 2026-08-30).
* `socket_gen` / `catalogue_digest` are optional (absent in pre-0.2.1 rows;
  readers must tolerate both shapes). `gen` is identity; the digest is
  informational — the live catalogue may have grown since the heartbeat
  (advertisement is a union; it only grows).
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

## 11. The script plane — runtime-attached predicates + aggregates (v1, 0.2.0)

The DTrace half. Scripts attach at runtime over the control socket, compile to an
in-process IR, and answer questions **where the events are born**. One semantics:
the whole clause compiles or attach fails with a precise error — there is no tier
in which the same text means something different.

### 11.1 The grammar (the whole language — one screen)

```
script   := PROBE ["where" expr] ["->" agg ("," agg)*] ["by" path ("," path)*]
            ["keep" path ("," path)*] ["ring" N]
agg      := "@" ("count" | ("sum"|"min"|"max"|"avg"|"quantize") expr)
expr     := comparisons (== != < <= > >=), arithmetic (+ - * /), && || ! parens
path     := bare = record header (`duration_ms` `component` `category` `name`
            `clock` `pid` `app` `ts_ms`) · `payload.x.y` = payload field
```

```sh
# slow frames: log2 latency histogram per host, since attach
ytrace attach --app yggterm 'render/gui where duration_ms > 16 -> @quantize(duration_ms) by payload.host_id'

# the µs-per-row slope case — arithmetic inside aggregate arguments
ytrace attach --app yggterm 'render/gui -> @quantize(duration_ms / payload.rows * 1000)'

# crime-scene capture: last 32 matching records, byte-capped, truncation is visible
ytrace attach --app yggterm 'daemon_terminal_read where payload.pending_chars == 0 keep payload, duration_ms ring 32'

ytrace scripts --app yggterm          # status + anti-false-zero counters
ytrace drain --app yggterm <id> --reset --watch 2   # live rate view
ytrace detach --app yggterm <id>
```

### 11.2 Laws (each one is load-bearing)

1. **Scripts see every firing, unsampled.** Sampling is a FILE-stream policy; a
   `@quantize` that saw 1:50 of fast frames would be a lying instrument.
2. **Attach is durable.** The CLI attaches and exits; aggregates accumulate until
   explicit detach or process death. Always-on instrumentation is the point.
3. **Drains ride the socket, never the plane.** Aggregate snapshots do not
   consume the JSONL byte budget — an instrument must not shorten the
   diagnostic window it exists to extend.
4. **Bounded by construction.** No loops, no user code. ≤1024 groups (+1 counted
   overflow bucket), ring ≤4096, captures >4 KiB truncate to a visible marker,
   script ≤4 KiB, ≤8 aggregates. Every bound reports its own overflow count.
5. **Anti-false-zero.** `fired / matched / schema_miss` are distinct stats:
   "probe never fired" ≠ "predicate never matched" ≠ "record didn't look the way
   the script assumed". A missing field or type mismatch bumps `schema_miss`
   and fails the predicate — never silent.

### 11.3 Non-goals (v1)

No loops, no variables, no user functions, no string transforms beyond equality,
no joins across probes (cross-probe correlation lives in the streaming sink or
ytop notebooks — join state is unbounded by nature and must not enter the
in-process VM). Probes remain statically declared; this is not uprobes.

### 11.4 Wire & protocol

The v1 record wire is frozen and untouched. Control socket: 
`$XDG_RUNTIME_DIR/ytrace/<app>-<pid>.sock` (advertised in the registry when
bound). Line-delimited JSON: `{"verb":"attach","id":opt,"script":"..."}` /
`{"verb":"detach","id"}` / `{"verb":"scripts"}` / `{"verb":"drain","id","reset"}`
/ `{"verb":"ping"}` / `{"verb":"catalogue"}`. Drain snapshots carry
`payload.aggregate`-shaped groups (`key`, `count`, `sum/avg/min/max`,
`quantize{min,max,p50,p95,p99,buckets}`) plus `ring` captures and the stats
block of §11.2.5.

**Identity + the attach canary (0.2.1).** `ping` answers
`{"ok":true,"gen":u64,"digest":u64,"probes_n":n,"app":...}`; `catalogue`
answers the full sorted probe list plus the same identity; a successful
`attach` echoes it. The CLI runs the canary BEFORE installing a script:

* registry `socket_gen` ≠ live `gen` → **refuse** (the socket is not the
  process the registry described — the shape that accepted attaches and then
  drained `fired=0/matched=0/schema_miss=0`, a confident false zero);
* the clause's probe absent from the live catalogue → **refuse** (the script
  would drain zero forever; the file plane remains a separate positive
  control via `ytrace query`);
* an old provider that answers no identity and rejects `catalogue` → attach
  proceeds **unverified** with a printed warning (mixed versions mid-roll
  must not hard-fail).

---

## 12. File map

```
~/gh/ytrace/
  docs/spec-ytrace.md              # this file
  crates/ytrace/src/
    lib.rs                         # Provider, Probe, SpanGuard, held-handle append, script hook
    script.rs                      # script IR, clause parser, aggregates, bounds
    control.rs                     # control socket server + client (attach/detach/drain)
    registry.rs                    # discovery heartbeat + prune
    query.rs                       # summarize / tail / incidents client
    compat.rs                      # yggterm/perf alias shim
  crates/ytrace/examples/mini_provider.rs   # live end-to-end demo
  .agents/skills/ytrace/SKILL.md   # agent dev integration guide
```
