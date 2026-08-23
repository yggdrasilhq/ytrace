//! ytrace diagnosis — pure, deterministic fault detection over samples.
//!
//! The daemon governor and ytop Dash share THIS logic, so a fault that the
//! daemon files is the same fault that Dash renders and an LLM can query via
//! `ytrace incidents`. No second encoding lives elsewhere.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Severity of an incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

/// Kind of incident — resource, health, or fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentKind {
    Resource,
    Health,
    Fault,
}

impl IncidentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IncidentKind::Resource => "resource",
            IncidentKind::Health => "health",
            IncidentKind::Fault => "fault",
        }
    }
}

/// One diagnosed incident — pure data, no I/O.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    /// Machine-readable id, e.g. "ssh_row_hot", "local_row_oom", "render_storm".
    pub id: String,
    pub kind: IncidentKind,
    pub severity: Severity,
    /// Human diagnosis sentence.
    pub diagnosis: String,
    /// Machine action taken or suggested.
    pub remedy: String,
    /// Observed value that tripped the threshold.
    pub observed: Value,
    /// Threshold that was crossed.
    pub threshold: Value,
    /// Row or host hint for attribution.
    pub subject: Option<String>,
    /// Suggested ytrace queries an LLM can run next.
    pub suggested_queries: Vec<String>,
}

/// Thresholds — pure constants, documented for unit-test stability.

pub const SSH_ROW_CORE_THRESHOLD: f64 = 0.80;
pub const SSH_ROW_SUSTAINED_SECS: u64 = 45;
pub const LOCAL_ROW_CORE_THRESHOLD: f64 = 0.90;
pub const LOCAL_ROW_MEM_KB_THRESHOLD: u64 = 1_500_000; // ~1.5 GB PSS
pub const LOCAL_ROW_SUSTAINED_SECS: u64 = 30;
pub const RENDER_TOTAL_CORE_THRESHOLD: f64 = 0.70;
pub const RENDER_SUSTAINED_SECS: u64 = 30;

/// A UI thread that has not proved it is running for this long is BLOCKED.
///
/// 200 ms is the point a human stops experiencing a keypress as immediate. It
/// is deliberately far below the seconds-long stalls that get reported, because
/// the useful record is the distribution of small blocks that precede a big one
/// — a freeze bad enough to be killed by hand has almost always been preceded
/// by a rising tail of short ones that nothing was watching.
pub const UI_BLOCK_THRESHOLD_MS: u64 = 200;
/// A block this long is a freeze a person notices and may act on.
pub const UI_BLOCK_SEVERE_MS: u64 = 1_000;
/// Blocks per minute above which the UI is not stalling but thrashing.
pub const UI_BLOCK_DENSITY_PER_MIN: f64 = 6.0;

// ── host panic thresholds ───────────────────────────────────────────────────
//
// These describe a CLIENT machine — the laptop a person is sitting in front of,
// not a compute host. The ordering below is deliberate and is the owner's:
// MEMORY first, then CPU, then space. A memory finding is never traded away for
// a CPU improvement.

/// Package temperature at which a laptop is audibly working.
pub const HOST_TEMP_WARN_C: f64 = 85.0;
/// Package temperature at which it is throttling and loud.
pub const HOST_TEMP_PANIC_C: f64 = 92.0;
/// Fraction of RAM in use before memory pressure is the headline.
pub const HOST_MEM_WARN_FRACTION: f64 = 0.85;
pub const HOST_MEM_PANIC_FRACTION: f64 = 0.93;
/// Swap in use, reported as CONTEXT and deliberately NOT an alarm trigger.
///
/// ⛔ **Swap-used is a LEVEL, not a pressure, and thresholding it produced an
/// alarm that could never clear.** After a memory crunch, GiBs stay resident in
/// swap while free RAM recovers, and nothing is obliged to reclaim them — that
/// is the normal steady state on a client that has been busy once. So the arm
/// was permanently true, the sustain requirement was trivially met, and
/// `host_panic_memory` fired every 60 s reading *"32% RAM in use, 7.5 GiB
/// swapped"* on a machine with 9 GiB free. Measured 2026-08-21: 118 of 449
/// incidents in a two-hour window — a quarter of the stream — were this alarm,
/// on retention that is a BYTE budget, so the noise was evicting the signal
/// people were reading it to find.
///
/// ⇒ **Only RATES belong in an alarm predicate.** A level says where the system
/// has been; a rate says what it is doing now, and only the second can clear.
/// The number is kept because it still EXPLAINS things — a slow first touch
/// after residency is real — and it still travels in `observed`.
pub const HOST_SWAP_CONTEXT_GIB: f64 = 4.0;
/// Cores our own GUI tree may burn before it is the thing making the noise.
pub const HOST_OUR_CORES_WARN: f64 = 1.0;
pub const HOST_OUR_CORES_PANIC: f64 = 2.0;
/// Bytes of `$XDG_RUNTIME_DIR` — a tmpfs, therefore RAM — we may hold.
///
/// 128 MiB in prod, 4 GiB in dev (2026-08-23): `YGGTERM_DEV=1` or
/// `~/.yggterm/config/dev-mode` contains `1` means the fleet dev hosts run
/// `yggterm-uglass` sway isolation on tmpfs (`/run/user/3001`), which
/// legitimately holds `mesa_shader_cache`/`uv`/`WPE` caches. The 128M threshold
/// was the unbounded-npm writer (1.5G x3 → 6.7G tmpfs); after deduplicating
/// uglass `npm` via symlink to `/home/pi/.yggterm/npm` (-4.3G → 2.4G), the
/// residual 2.x GiB is sway/WPE caches, not an yggterm leak. Dev threshold
/// 4G still catches the leak (1.5G x3 would be 4.5G) without firing on warm
/// caches.
pub const HOST_RUNTIME_TMPFS_PANIC_BYTES: u64 = 128 * 1024 * 1024;
pub const HOST_RUNTIME_TMPFS_PANIC_BYTES_DEV: u64 = 4 * 1024 * 1024 * 1024;

fn host_runtime_tmpfs_panic_bytes() -> u64 {
    if std::env::var("YGGTERM_DEV")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return HOST_RUNTIME_TMPFS_PANIC_BYTES_DEV;
    }
    if let Some(home) = dirs::home_dir() {
        if let Ok(c) = std::fs::read_to_string(home.join(".yggterm/config/dev-mode")) {
            let t = c.trim().to_ascii_lowercase();
            if t == "1" || t == "true" || t == "yes" {
                return HOST_RUNTIME_TMPFS_PANIC_BYTES_DEV;
            }
        }
    }
    HOST_RUNTIME_TMPFS_PANIC_BYTES
}
/// How long a condition must hold before it is a panic rather than a spike.
pub const HOST_PANIC_SUSTAINED_SECS: u64 = 60;

/// Verdict for one row's resource sample.
#[derive(Debug, Clone)]
pub struct RowResourceSample {
    pub row_id: String,
    /// Is this an SSH row? SSH → detach path, Local → telemetry only.
    pub is_ssh: bool,
    pub core_fraction: f64,
    pub mem_kb: Option<u64>,
    pub duration_secs: u64,
}

/// One observed stall of the UI thread.
///
/// ⛔ This sample can only be taken by a thread that is NOT the UI thread. The
/// probes that would normally record a freeze — the input chain, the render
/// counter, every handler-side event — all run ON the thread that is frozen, so
/// during a real stall they emit nothing and the incident log reads clean. A
/// freeze measured from inside the freeze is always zero. The watchdog must
/// therefore observe a heartbeat the UI thread stamps, from outside it.
#[derive(Debug, Clone)]
pub struct UiBlockSample {
    /// How long the UI thread went without proving it was alive.
    pub gap_ms: u64,
    /// The last activity recorded before the gap opened — best-effort attribution.
    pub last_activity: Option<String>,
    /// Blocks seen per minute over the recent window, for the thrashing verdict.
    pub blocks_per_min: Option<f64>,
}

/// One sample of a client host's overall state.
///
/// Every field is optional because an unreadable sensor must stay unreadable.
/// A missing temperature is not a cool machine, and defaulting it to zero turns
/// a blind instrument into an all-clear — which is precisely the failure this
/// whole detector exists to end.
#[derive(Debug, Clone, Default)]
pub struct HostPanicSample {
    pub host: String,
    pub package_temp_c: Option<f64>,
    pub mem_used_fraction: Option<f64>,
    pub swap_used_gib: Option<f64>,
    /// Cores burned by OUR process tree (GUI + webview), not by the machine.
    pub our_cores: Option<f64>,
    pub runtime_tmpfs_bytes: Option<u64>,
    pub ui_blocks_per_min: Option<f64>,
    /// How long the offending condition has held.
    pub sustained_secs: u64,
    /// Row to address the notification to, when one is known.
    pub subject_row: Option<String>,
}

/// Verdict for the whole render tree (gui + web).
#[derive(Debug, Clone)]
pub struct RenderSample {
    pub total_core_fraction: f64,
    pub duration_secs: u64,
}

/// Pure: row → optional incident.
pub fn diagnose_row(sample: &RowResourceSample) -> Option<Incident> {
    if sample.is_ssh {
        if sample.core_fraction >= SSH_ROW_CORE_THRESHOLD
            && sample.duration_secs >= SSH_ROW_SUSTAINED_SECS
        {
            return Some(Incident {
                id: "ssh_row_hot".to_string(),
                kind: IncidentKind::Resource,
                severity: Severity::Warn,
                diagnosis: format!(
                    "SSH row {} sustained {:.0}% core for {}s — likely runaway remote process",
                    sample.row_id,
                    sample.core_fraction * 100.0,
                    sample.duration_secs
                ),
                remedy: "detached SSH control-master; row preserved, reattach in 120s".to_string(),
                observed: json!({"core_fraction": sample.core_fraction, "duration_secs": sample.duration_secs}),
                threshold: json!({"core_fraction": SSH_ROW_CORE_THRESHOLD, "duration_secs": SSH_ROW_SUSTAINED_SECS}),
                subject: Some(sample.row_id.clone()),
                suggested_queries: vec![
                    format!("ytrace query --app yggterm --category row_resource --name {} --since 5m --json", sample.row_id),
                    "ytrace incidents --app yggterm --since 1h --json".to_string(),
                ],
            });
        }
    } else {
        let hot = sample.core_fraction >= LOCAL_ROW_CORE_THRESHOLD
            && sample.duration_secs >= LOCAL_ROW_SUSTAINED_SECS;
        let oom = sample
            .mem_kb
            .map(|kb| kb >= LOCAL_ROW_MEM_KB_THRESHOLD)
            .unwrap_or(false)
            && sample.duration_secs >= LOCAL_ROW_SUSTAINED_SECS;
        if hot || oom {
            let reason = if hot && oom {
                "CPU+memory"
            } else if hot {
                "CPU"
            } else {
                "memory"
            };
            return Some(Incident {
                id: if oom { "local_row_oom".to_string() } else { "local_row_hot".to_string() },
                kind: IncidentKind::Resource,
                severity: Severity::Error,
                diagnosis: format!(
                    "Local row {} {} saturated {:.0}% core / {} MB for {}s",
                    sample.row_id,
                    reason,
                    sample.core_fraction * 100.0,
                    sample.mem_kb.unwrap_or(0) / 1024,
                    sample.duration_secs
                ),
                remedy: "telemetry logged; row kept alive — inspect via ytop Dash".to_string(),
                observed: json!({"core_fraction": sample.core_fraction, "mem_kb": sample.mem_kb, "duration_secs": sample.duration_secs}),
                threshold: json!({"core_fraction": LOCAL_ROW_CORE_THRESHOLD, "mem_kb": LOCAL_ROW_MEM_KB_THRESHOLD, "duration_secs": LOCAL_ROW_SUSTAINED_SECS}),
                subject: Some(sample.row_id.clone()),
                suggested_queries: vec![
                    format!("ytrace tail --app yggterm --since 200 --json | grep {}", sample.row_id),
                ],
            });
        }
    }
    None
}

/// Pure: render tree → optional incident.
pub fn diagnose_render(sample: &RenderSample) -> Option<Incident> {
    if sample.total_core_fraction >= RENDER_TOTAL_CORE_THRESHOLD
        && sample.duration_secs >= RENDER_SUSTAINED_SECS
    {
        return Some(Incident {
            id: "render_storm".to_string(),
            kind: IncidentKind::Fault,
            severity: Severity::Warn,
            diagnosis: format!(
                "GUI+WebKit sustain {:.0}% core for {}s — render storm",
                sample.total_core_fraction * 100.0,
                sample.duration_secs
            ),
            remedy: "forward throttled; check viewport bootstrap / protocol guard".to_string(),
            observed: json!({"core_fraction": sample.total_core_fraction, "duration_secs": sample.duration_secs}),
            threshold: json!({"core_fraction": RENDER_TOTAL_CORE_THRESHOLD, "duration_secs": RENDER_SUSTAINED_SECS}),
            subject: None,
            suggested_queries: vec![
                "ytrace query --app yggterm --category render --since 1m --json".to_string(),
                "ytop --probe <host> --json | head -n 40".to_string(),
            ],
        });
    }
    None
}

/// Pure: one UI-thread stall -> optional incident.
pub fn diagnose_ui_block(sample: &UiBlockSample) -> Option<Incident> {
    if sample.gap_ms < UI_BLOCK_THRESHOLD_MS {
        return None;
    }
    let severe = sample.gap_ms >= UI_BLOCK_SEVERE_MS;
    let thrashing = sample
        .blocks_per_min
        .map(|d| d >= UI_BLOCK_DENSITY_PER_MIN)
        .unwrap_or(false);
    let attributed = sample
        .last_activity
        .clone()
        .unwrap_or_else(|| "unattributed".to_string());
    Some(Incident {
        id: if severe { "ui_block_severe".to_string() } else { "ui_block".to_string() },
        kind: IncidentKind::Fault,
        severity: if severe { Severity::Error } else { Severity::Warn },
        diagnosis: format!(
            "UI thread stalled {} ms (last activity before the gap: {})",
            sample.gap_ms, attributed
        ),
        remedy: if severe {
            "a stall this long is a visible freeze — move the named work off the UI thread"
                .to_string()
        } else {
            "short stall recorded; watch the density, a rising tail precedes a freeze".to_string()
        },
        observed: json!({
            "gap_ms": sample.gap_ms,
            "last_activity": sample.last_activity,
            "blocks_per_min": sample.blocks_per_min,
            "thrashing": thrashing,
        }),
        threshold: json!({
            "gap_ms": UI_BLOCK_THRESHOLD_MS,
            "severe_ms": UI_BLOCK_SEVERE_MS,
            "density_per_min": UI_BLOCK_DENSITY_PER_MIN,
        }),
        subject: sample.last_activity.clone(),
        suggested_queries: vec![
            "ytrace incidents --app yggterm --since 1h --json".to_string(),
            "ytrace query --app yggterm --category ui --name block --since 1h --json".to_string(),
        ],
    })
}

/// Pure: a client host's state -> optional panic incident.
///
/// Returns at most ONE incident, naming the single worst thing. A detector that
/// files one incident per crossed threshold turns a hot afternoon into five
/// notifications that all say the same thing, and the reader learns to dismiss
/// the channel. The reasons are evaluated in the owner's priority order —
/// memory, then CPU, then heat — so the headline is the one that matters most,
/// and the rest travel in `observed` where they can still be read.
pub fn diagnose_host_panic(sample: &HostPanicSample) -> Option<Incident> {
    if sample.sustained_secs < HOST_PANIC_SUSTAINED_SECS {
        return None;
    }

    let observed = json!({
        "host": sample.host,
        "package_temp_c": sample.package_temp_c,
        "mem_used_fraction": sample.mem_used_fraction,
        "swap_used_gib": sample.swap_used_gib,
        "our_cores": sample.our_cores,
        "runtime_tmpfs_bytes": sample.runtime_tmpfs_bytes,
        "ui_blocks_per_min": sample.ui_blocks_per_min,
        "sustained_secs": sample.sustained_secs,
    });
    let threshold = json!({
        "temp_panic_c": HOST_TEMP_PANIC_C,
        "mem_panic_fraction": HOST_MEM_PANIC_FRACTION,
        // Named `_context_` and not `_panic_`, because a reader who sees a
        // number under "threshold" reasonably assumes crossing it fires.
        "swap_context_gib": HOST_SWAP_CONTEXT_GIB,
        "our_cores_panic": HOST_OUR_CORES_PANIC,
        "runtime_tmpfs_panic_bytes": host_runtime_tmpfs_panic_bytes(),
        "ui_block_density_per_min": UI_BLOCK_DENSITY_PER_MIN,
        "sustained_secs": HOST_PANIC_SUSTAINED_SECS,
    });

    // MEMORY FIRST.
    let (id, severity, headline, remedy) = if sample
        .runtime_tmpfs_bytes
        .map(|b| b >= host_runtime_tmpfs_panic_bytes())
        .unwrap_or(false)
    {
        let mib = sample.runtime_tmpfs_bytes.unwrap_or(0) / (1024 * 1024);
        (
            "host_panic_tmpfs",
            Severity::Error,
            format!("{mib} MiB held in the runtime tmpfs — that is resident memory, not disk"),
            "find the unbounded writer under $XDG_RUNTIME_DIR and give it a retention budget",
        )
    // ⛔ RAM-in-use ONLY. Swap residency is NOT ORed in here: see
    // [`HOST_SWAP_CONTEXT_GIB`] — it is a level that never falls on its own, so
    // as a trigger it made an alarm that no action by the machine could clear.
    } else if sample
        .mem_used_fraction
        .map(|f| f >= HOST_MEM_PANIC_FRACTION)
        .unwrap_or(false)
    {
        (
            "host_panic_memory",
            Severity::Error,
            format!(
                "memory pressure: {:.0}% RAM in use, {:.1} GiB swapped",
                sample.mem_used_fraction.unwrap_or(0.0) * 100.0,
                sample.swap_used_gib.unwrap_or(0.0)
            ),
            "shed the largest resident consumer before touching anything else",
        )
    // THEN CPU — and ours specifically, because that is the part we can fix.
    } else if sample.our_cores.map(|c| c >= HOST_OUR_CORES_PANIC).unwrap_or(false) {
        (
            "host_panic_our_cpu",
            Severity::Error,
            format!(
                "our own GUI tree is burning {:.2} cores on the client",
                sample.our_cores.unwrap_or(0.0)
            ),
            "this is a render or chore storm we own — see the render-storm notebook",
        )
    } else if sample
        .ui_blocks_per_min
        .map(|d| d >= UI_BLOCK_DENSITY_PER_MIN)
        .unwrap_or(false)
    {
        (
            "host_panic_ui_thrash",
            Severity::Error,
            format!(
                "UI thread blocking {:.0} times a minute — the interface is thrashing",
                sample.ui_blocks_per_min.unwrap_or(0.0)
            ),
            "read the ui/block attributions; a chore is running on the UI thread",
        )
    // THEN HEAT, which is usually a consequence of the above rather than a cause.
    } else if sample.package_temp_c.map(|t| t >= HOST_TEMP_PANIC_C).unwrap_or(false) {
        (
            "host_panic_thermal",
            Severity::Error,
            format!(
                "package at {:.0} C — the machine is throttling and loud",
                sample.package_temp_c.unwrap_or(0.0)
            ),
            "attribute by SHARE OF BUSY CPU, never by count of idle cores",
        )
    } else if sample.package_temp_c.map(|t| t >= HOST_TEMP_WARN_C).unwrap_or(false)
        || sample.our_cores.map(|c| c >= HOST_OUR_CORES_WARN).unwrap_or(false)
        || sample
            .mem_used_fraction
            .map(|f| f >= HOST_MEM_WARN_FRACTION)
            .unwrap_or(false)
    {
        (
            "host_warm",
            Severity::Warn,
            "client host is working hard but below the panic thresholds".to_string(),
            "watch; no action yet",
        )
    } else {
        return None;
    };

    Some(Incident {
        id: id.to_string(),
        kind: IncidentKind::Health,
        severity,
        diagnosis: format!("{}: {headline}", sample.host),
        remedy: remedy.to_string(),
        observed,
        threshold,
        subject: sample.subject_row.clone(),
        suggested_queries: vec![
            "ytrace incidents --app yggterm --since 1h --json".to_string(),
            "ytrace query --app yggterm --category ui --name block --since 1h --json".to_string(),
            "ytrace query --app yggterm --category render --since 5m --json".to_string(),
        ],
    })
}

/// Render an incident into the ytrace payload shape (incident=true).
pub fn incident_payload(incident: &Incident) -> Value {
    json!({
        "incident": true,
        "incident_id": incident.id,
        "kind": incident.kind.as_str(),
        "severity": incident.severity.as_str(),
        "diagnosis": incident.diagnosis,
        "remedy": incident.remedy,
        "observed": incident.observed,
        "threshold": incident.threshold,
        "subject": incident.subject,
        "suggested_queries": incident.suggested_queries,
        "complaint_for": "llm",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_hot_triggers_warn_and_suggests_detach() {
        let s = RowResourceSample {
            row_id: "42".to_string(),
            is_ssh: true,
            core_fraction: 0.85,
            mem_kb: None,
            duration_secs: 60,
        };
        let inc = diagnose_row(&s).expect("should trigger");
        assert_eq!(inc.id, "ssh_row_hot");
        assert_eq!(inc.severity, Severity::Warn);
        assert!(inc.remedy.contains("detached"));
    }

    #[test]
    fn ssh_below_threshold_no_incident() {
        let s = RowResourceSample {
            row_id: "1".to_string(),
            is_ssh: true,
            core_fraction: 0.5,
            mem_kb: None,
            duration_secs: 60,
        };
        assert!(diagnose_row(&s).is_none());
    }

    #[test]
    fn local_hot_triggers_error() {
        let s = RowResourceSample {
            row_id: "local-1".to_string(),
            is_ssh: false,
            core_fraction: 0.95,
            mem_kb: Some(100_000),
            duration_secs: 40,
        };
        let inc = diagnose_row(&s).expect("hot");
        assert_eq!(inc.severity, Severity::Error);
    }

    #[test]
    fn local_oom_triggers() {
        let s = RowResourceSample {
            row_id: "local-2".to_string(),
            is_ssh: false,
            core_fraction: 0.1,
            mem_kb: Some(2_000_000),
            duration_secs: 35,
        };
        let inc = diagnose_row(&s).expect("oom");
        assert_eq!(inc.id, "local_row_oom");
    }

    #[test]
    fn render_storm_triggers_warn() {
        let s = RenderSample {
            total_core_fraction: 0.9,
            duration_secs: 60,
        };
        let inc = diagnose_render(&s).expect("storm");
        assert_eq!(inc.id, "render_storm");
    }

    #[test]
    fn render_cool_no_incident() {
        let s = RenderSample {
            total_core_fraction: 0.3,
            duration_secs: 60,
        };
        assert!(diagnose_render(&s).is_none());
    }

    #[test]
    fn ui_block_below_threshold_is_not_an_incident() {
        let s = UiBlockSample { gap_ms: 120, last_activity: None, blocks_per_min: None };
        assert!(diagnose_ui_block(&s).is_none(), "a 120 ms gap is ordinary scheduling");
    }

    #[test]
    fn ui_block_names_what_ran_before_the_gap() {
        let s = UiBlockSample {
            gap_ms: 640,
            last_activity: Some("copy_generation/title".to_string()),
            blocks_per_min: Some(2.0),
        };
        let inc = diagnose_ui_block(&s).expect("640 ms is a block");
        assert_eq!(inc.id, "ui_block");
        assert_eq!(inc.severity, Severity::Warn);
        assert!(
            inc.diagnosis.contains("copy_generation/title"),
            "a block that cannot name what ran before it is the bug this probe exists to fix"
        );
        assert_eq!(inc.subject.as_deref(), Some("copy_generation/title"));
    }

    #[test]
    fn a_visible_freeze_is_an_error_not_a_warning() {
        let s = UiBlockSample { gap_ms: 5_000, last_activity: None, blocks_per_min: None };
        let inc = diagnose_ui_block(&s).expect("5 s is a freeze");
        assert_eq!(inc.id, "ui_block_severe");
        assert_eq!(inc.severity, Severity::Error);
        assert_eq!(inc.observed["last_activity"], serde_json::Value::Null);
        assert!(inc.diagnosis.contains("unattributed"));
    }

    #[test]
    fn ui_block_density_marks_thrashing() {
        let s = UiBlockSample {
            gap_ms: 250,
            last_activity: Some("sidebar/merge_rows".to_string()),
            blocks_per_min: Some(UI_BLOCK_DENSITY_PER_MIN + 1.0),
        };
        let inc = diagnose_ui_block(&s).expect("block");
        assert_eq!(inc.observed["thrashing"], true);
    }

    fn hot_sample() -> HostPanicSample {
        HostPanicSample {
            host: "example-client".to_string(),
            sustained_secs: HOST_PANIC_SUSTAINED_SECS,
            ..Default::default()
        }
    }

    #[test]
    fn a_spike_shorter_than_the_sustain_window_is_not_a_panic() {
        let s = HostPanicSample {
            package_temp_c: Some(99.0),
            sustained_secs: 5,
            ..hot_sample()
        };
        assert!(diagnose_host_panic(&s).is_none(), "a five-second spike is not a panic");
    }

    #[test]
    fn an_unreadable_sensor_never_reads_as_healthy() {
        // Every field None: nothing crossed, but nothing was measured either.
        // The detector must stay silent rather than assert a clean bill of health.
        let s = hot_sample();
        assert!(diagnose_host_panic(&s).is_none());
        // And a blind temperature must not suppress a real memory finding.
        let s = HostPanicSample {
            package_temp_c: None,
            mem_used_fraction: Some(0.97),
            ..hot_sample()
        };
        let inc = diagnose_host_panic(&s).expect("memory pressure is visible without a thermometer");
        assert_eq!(inc.id, "host_panic_memory");
    }

    #[test]
    fn memory_outranks_heat_when_both_are_crossed() {
        // The owner's priority order, encoded: MEMORY > CPU > SPACE. A machine
        // that is both hot and out of memory reports the memory.
        let s = HostPanicSample {
            package_temp_c: Some(99.0),
            mem_used_fraction: Some(0.99),
            our_cores: Some(3.0),
            ..hot_sample()
        };
        let inc = diagnose_host_panic(&s).expect("panic");
        assert_eq!(inc.id, "host_panic_memory", "memory is the headline, not the heat");
        // and the rest still travel in the payload
        assert_eq!(inc.observed["package_temp_c"], 99.0);
        assert_eq!(inc.observed["our_cores"], 3.0);
    }

    /// ⛔ THE ALARM THAT COULD NEVER CLEAR. Swap residency is a LEVEL: after one
    /// memory crunch it stays high while RAM recovers, so an arm thresholding it
    /// is true forever and the sustain window is met trivially. This is the
    /// regression lock — a machine with plenty of free RAM and a swapfile full
    /// of yesterday's residue is HEALTHY, and must be reported as such.
    #[test]
    fn swap_residency_alone_is_never_a_memory_panic() {
        let s = HostPanicSample {
            mem_used_fraction: Some(0.32),
            swap_used_gib: Some(7.5),
            ..hot_sample()
        };
        assert!(
            diagnose_host_panic(&s).is_none(),
            "32% RAM in use with 7.5 GiB of swap RESIDUE is not memory pressure — \
             thresholding it fired every 60s on a machine with 9 GiB free"
        );

        // The control, so this cannot pass by disabling the arm outright:
        // genuine RAM exhaustion is still a panic, and swap still travels as
        // context in the payload.
        let s = HostPanicSample {
            mem_used_fraction: Some(HOST_MEM_PANIC_FRACTION + 0.01),
            swap_used_gib: Some(7.5),
            ..hot_sample()
        };
        let inc = diagnose_host_panic(&s).expect("real RAM exhaustion is still a panic");
        assert_eq!(inc.id, "host_panic_memory");
        assert_eq!(
            inc.observed["swap_used_gib"], 7.5,
            "swap stays in the payload — it still EXPLAINS a slow first touch"
        );
    }

    #[test]
    fn only_one_incident_is_ever_returned() {
        let s = HostPanicSample {
            package_temp_c: Some(120.0),
            mem_used_fraction: Some(0.99),
            swap_used_gib: Some(12.0),
            our_cores: Some(9.0),
            runtime_tmpfs_bytes: Some(u64::MAX),
            ui_blocks_per_min: Some(99.0),
            ..hot_sample()
        };
        let inc = diagnose_host_panic(&s).expect("panic");
        assert_eq!(inc.id, "host_panic_tmpfs", "the single worst thing, named once");
    }

    #[test]
    fn sustained_ui_block_density_is_itself_a_panic() {
        let s = HostPanicSample {
            ui_blocks_per_min: Some(UI_BLOCK_DENSITY_PER_MIN + 4.0),
            subject_row: Some("row-under-test".to_string()),
            ..hot_sample()
        };
        let inc = diagnose_host_panic(&s).expect("thrashing is a panic");
        assert_eq!(inc.id, "host_panic_ui_thrash");
        assert_eq!(inc.subject.as_deref(), Some("row-under-test"),
                   "a notification is an ADDRESS — it must know which row to land on");
    }

    #[test]
    fn a_warm_machine_warns_without_panicking() {
        let s = HostPanicSample {
            package_temp_c: Some(HOST_TEMP_WARN_C + 1.0),
            ..hot_sample()
        };
        let inc = diagnose_host_panic(&s).expect("warm");
        assert_eq!(inc.id, "host_warm");
        assert_eq!(inc.severity, Severity::Warn);
    }

    #[test]
    fn incident_payload_marks_complaint_for_llm() {
        let inc = Incident {
            id: "x".into(),
            kind: IncidentKind::Fault,
            severity: Severity::Warn,
            diagnosis: "test".into(),
            remedy: "fix".into(),
            observed: json!({}),
            threshold: json!({}),
            subject: None,
            suggested_queries: vec![],
        };
        let p = incident_payload(&inc);
        assert_eq!(p["incident"], true);
        assert_eq!(p["complaint_for"], "llm");
    }
}
