//! The gpt-tracing audit's critical finding, as a runnable end-to-end demo:
//! ONE process builds FOUR providers of one app (yggterm builds exactly this
//! many: narrative trace, perf, row governor, host panic). Before the
//! 2026-08-30 fix each one rebound `<app>-<pid>.sock`, orphaning the previous
//! listener and flipping the registry catalogue; now they all JOIN one control
//! plane and the registry row carries the union plus the engine identity.
//!
//! ```sh
//! cargo run --example twin_providers &
//! ytrace registry                          # one row, gen + union catalogue
//! ytrace attach --app twindemo 'perf/request -> @count'      # accepted
//! ytrace attach --app twindemo 'daemon_request/snapshot -> @count'  # REFUSED before the fix
//! ```

use serde_json::json;
use std::time::Duration;
use ytrace::{Clock, Provider, Sample};

fn main() {
    let trace = Provider::new("twindemo", "0.0.0-demo");
    let perf = Provider::new("twindemo", "0.0.0-demo");
    let governor = Provider::new("twindemo", "0.0.0-demo");
    let panic = Provider::new("twindemo", "0.0.0-demo");

    trace.register("trace/gui", Clock::Wall, Sample::always());
    perf.register("perf/request", Clock::Wall, Sample::always());
    governor.register("governor/fault", Clock::Wall, Sample::always());
    panic.register("host/panic", Clock::Wall, Sample::always());

    let sockets = [
        trace.socket_path(),
        perf.socket_path(),
        governor.socket_path(),
        panic.socket_path(),
    ];
    println!("socket per provider: {sockets:?}");
    assert!(
        sockets.iter().all(|s| s == &sockets[0]),
        "all providers must share ONE socket"
    );
    println!(
        "engine gen: {}  catalogue: {:?}",
        perf.control().gen(),
        perf.control().catalogue()
    );

    // the four providers emit on rotation, forever (Ctrl-C to stop)
    let mut i: u64 = 0;
    loop {
        let n = i % 4;
        match n {
            0 => perf.emit_span("p", "perf", "request", Clock::Wall, 3.0, json!({"op": i})),
            1 => governor.incident("g", "governor", "fault", json!({"why": "demo"})),
            2 => panic.event("h", "host", "panic", json!({"heartbeat": i})),
            _ => trace.event("t", "trace", "gui", json!({"tick": i})),
        }
        i += 1;
        std::thread::sleep(Duration::from_millis(400));
    }
}
