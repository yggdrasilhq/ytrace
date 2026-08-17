---
name: ytrace
description: DTrace-like probe bus for ytop fleet — how to emit spans/events/metrics in any app (yggterm, ytop, ychrome, proprietary) and query them from ytop.
---

# ytrace — fleet probe integration

Use when wiring `ytrace` into an app so `ytop` can introspect it like `yggterm`, or when querying the fleet from `ytop`.

## When to use
- App emits `span`/`event`/`metric`/`incident` → `ytop` Top/Dash, `ytrace query` CLI, eBPF opt-in
- App is `yggterm`, `ytop`, `ychrome`, `yedit`, or a proprietary webapp/Python service
- You need DTrace-like `provider/module/probe` without linking `libyggterm`

## Spec
`~/gh/ytrace/docs/spec-ytrace.md` is SSOT — wire `v:1 {ts_ms,pid,app,app_version,component,category,name,clock,duration_ms,payload}`, clocks `wall|cpu`, sampling `floor 8ms 1:50`, transport file `$XDG_DATA_HOME/ytrace/<app>/ytrace.jsonl` (+ generations) + live `$XDG_RUNTIME_DIR/ytrace/<app>-<pid>.sock` + registry `$XDG_RUNTIME_DIR/ytrace/registry.jsonl` (45s stale).

## 5-line provider (Rust)
```toml
[dependencies]
ytrace = { git = "https://github.com/yggdrasilhq/ytrace" }
```
```rust
use ytrace::{Provider, Clock, Sample};
use serde_json::json;
static YT: std::sync::LazyLock<Provider> = std::sync::LazyLock::new(|| Provider::new("myapp", env!("CARGO_PKG_VERSION")));
YT.register("http/fetch", Clock::Wall, Sample::always());
let span = YT.span("web", "http", "fetch", json!({"url": url}));
// ... work ...
span.finish(json!({"status":200}));
YT.event("cache","cache","evict", json!({"keys":3}));
```

## Query (ytop / CLI)
```sh
# via ytrace client (ytop fan-out stdin-fed, never argv-smuggling)
ytrace query --app yggterm --category daemon_request --name status --since 60s --top 10 --json
ytrace query --app yggterm --category render --json   # cpu clock, gpu_ms in payload
ytrace tail --app yggterm --since 200
ytrace incidents --app yggterm --since 1h
ytrace tenants --app yggterm  # live row tenant probe wrapper
ytrace registry --list --stale 45s
```
- `is_enabled()` is one relaxed atomic; `SpanGuard` early-returns when `YTRACE_ENABLED=0` or settings off.
- `Provider::with_home(app, ver, path)` + `YTRACE_HOME` override for proprietary ` /var/log/ytrace`.
- Compat: `ytrace::compat::yggterm_home()` aliases `~/.yggterm/perf-telemetry.jsonl` during migration.

## yggterm → ytrace map (first provider)
- `perf.rs PerfSpan/PerfGuard` → `ytrace span category/name wall` (sampling via `Probe.sample`)
- `trace.rs EventTraceRecord` → `ytrace event component/category/name wall`
- `render_probe RenderProbe` → `ytrace metric render/gui cpu {cores,gpu_ms}`

## ytop → ytrace map (first consumer)
- `probe.rs` 400ms `/proc` delta → keep for host `Top`; fleet `Dash` reads `ytrace::registry::list()` + `query::summarize()` over ssh stdin-fed fan-out
- `rows.rs tenants` + `timeline Ring` → ytrace `query::tail` + registry heartbeat

## Verification
`cargo -p ytrace check/test` green; `ytrace query --app yggterm` vs `yggterm-headless server perf-summary --category render` agree within 1.4%.

## Notebooks — Dash is exclusively ytrace (book pages in ytop sidebar)

`ytop` notebooks are the book interface for ytrace: **Top** shelf has no ytrace (host atlas), **Dash** shelf is exclusively ytrace profiling adventures. Any agent on any host composes via `ytop` skill `POST /action notebook_compose_{top,dash}` (stdin-fed, never argv-joined) → `~/.local/share/ytop/notebooks/<id>.json`. See `ytop/.agents/skills/ytop-notebooks/SKILL.md` and `ytop/src/notebook.rs` (base notebooks: `top-atlas-jojo`, `dash-angry-gui`, `dash-idle-cost`).
