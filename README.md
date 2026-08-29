# ytrace — DTrace-like probes for the ytop fleet

One probe surface, many apps. `ytrace` is a tiny Rust lib (zero deps beyond `serde`) that any app (`yggterm`, `ytop`, `ychrome`, `yedit`, proprietary `dioxus`/`yggui` webapps — free or proprietary) embeds to emit **spans / events / metrics / incidents**, query them file-first, and — since 0.2.0 — run **DTrace-class scripts**: runtime-attached clauses with predicates and in-process aggregates (`@quantize`, `@count`, rings), evaluated at the probe site with negligible overhead, drained over a control socket that never consumes the byte budget. `ytop` discovers all providers via a shared registry and queries them with the same verbs. It replaces all hand-rolled probing (`ygg-panic` hourly watch, `usability-check`, `perf-telemetry`/`render_probe`/`resource-recorder`) — later you only `ytop` the probes you want.

```sh
ytrace attach --app yggterm 'render/gui where duration_ms > 16 -> @quantize(duration_ms) by payload.host_id'
ytrace drain --app yggterm render/gui@quantize --watch 2   # live p50/p95/max per host
```

> **One `ytop` sees five planes at once:** the server machine(s), the client machine you’re looking from, the `yggterm` terminal fleet, the `ychrome` browser surface, and the webapp in the viewport. A hitch in the app’s `fetch` span, a `render/gui` storm, a `zfs_delay` outlier, and a `media_capture` prompt all land as the same `ytrace` record kind, queried the same way, in the same notebook — so a frontend jank is correlated to a ZFS commit without switching tools.

`ytop` ships base notebooks for both Top and Dash — a few per mode as needed, not one each — each reading `ytrace` via `ytrace query/tail/incidents`.

Spec: [`docs/spec-ytrace.md`](docs/spec-ytrace.md) — wire, clocks, sampling, transport, discovery, retention, query.

```
myapp ──► ytrace span/event ──► $XDG_DATA_HOME/ytrace/myapp/ytrace.jsonl (+ generations)
                           ──► $XDG_RUNTIME_DIR/ytrace/registry.jsonl (heartbeat)
ytop  ──► ytrace::registry::list() ──► ytrace::query::summarize() / tail()
```

Quick start:

```rust
use ytrace::{Provider, Clock, Sample};
use serde_json::json;

static YT: std::sync::LazyLock<Provider> =
    std::sync::LazyLock::new(|| Provider::new("myapp", env!("CARGO_PKG_VERSION")));

YT.register("http/fetch", Clock::Wall, Sample::always());

let span = YT.span("web", "http", "fetch", json!({"url": url}));
// ... work ...
span.finish(json!({"status": 200}));
```

See `Provider`, `SpanGuard`, `Clock`, `Sample` in `crates/ytrace/src/lib.rs`.
