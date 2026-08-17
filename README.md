# ytrace — DTrace-like probes for the ytop fleet

One probe surface, many apps. `ytrace` is a tiny Rust lib (zero deps beyond `serde`) that any app (`yggterm`, `ytop`, `ychrome`, `yedit`, proprietary webapps) embeds to emit **spans / events / metrics / incidents**. `ytop` discovers all providers via a shared registry and queries them with the same verbs.

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
