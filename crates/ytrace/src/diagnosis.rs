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
