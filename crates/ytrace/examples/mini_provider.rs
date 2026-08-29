//! A minimal live provider for exercising the script plane end-to-end:
//!
//! ```sh
//! cargo run --example mini_provider &
//! ytrace attach --app minidemo 'render/frame where duration_ms > 16 -> @quantize(duration_ms), @count by payload.host_id keep duration_ms ring 8' --watch 1
//! ytrace drain --app minidemo render/frame@quantize
//! ```

use serde_json::json;
use std::time::Duration;
use ytrace::{Clock, Provider, Sample};

static YT: std::sync::LazyLock<Provider> =
    std::sync::LazyLock::new(|| Provider::new("minidemo", "0.0.0-demo"));

fn main() {
    YT.register("render/frame", Clock::Wall, Sample::noisy());
    if let Some(sock) = YT.socket_path() {
        println!("control socket: {}", sock.display());
    }
    println!("home: {}", YT.home().display());
    let mut i: u64 = 0;
    loop {
        // mostly fast frames; every 7th is a slow outlier; two hosts alternate
        let dur = if i % 7 == 0 { 20.0 + (i % 5) as f64 * 9.0 } else { 2.0 + (i % 4) as f64 };
        let host = if i % 2 == 0 { "terminal-a" } else { "terminal-b" };
        let span = YT.span("ui", "render", "frame", json!({"host_id": host}));
        std::thread::sleep(Duration::from_millis(dur as u64));
        span.finish(json!({"rows": 54, "frame_no": i}));
        i += 1;
        std::thread::sleep(Duration::from_millis(20));
    }
}
