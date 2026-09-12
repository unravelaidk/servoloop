use crate::output::{redact, redact_value, RunOutput};
use serde_json::Value;
use servoloop_core::Event;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

const MAX_ACTIVITY: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Ready,
    Running,
    Cancelling,
    Complete,
    Interrupted,
    Failed,
}

impl Phase {
    pub(super) fn active(self) -> bool {
        matches!(self, Self::Running | Self::Cancelling)
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready / nothing dispatched",
            Self::Running => "Running / awaiting evidence",
            Self::Cancelling => "Cancelling / awaiting cleanup",
            Self::Complete => "Complete / snapshot saved",
            Self::Interrupted => "Interrupted / inspect before continuing",
            Self::Failed => "Failed / inspect before continuing",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Report {
    pub(super) demo: bool,
    pub(super) answer: String,
    pub(super) post_observed: Option<f64>,
    pub(super) phase: Phase,
    pub(super) session: Option<String>,
    pub(super) store: String,
    pub(super) initial: Option<f64>,
    pub(super) verified: Option<f64>,
    pub(super) journal_recorded: bool,
    pub(super) snapshot_saved: bool,
    pub(super) command_started: bool,
    pub(super) activity: VecDeque<String>,
    pub(super) omitted: usize,
    pub(super) error: Option<String>,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            demo: true,
            answer: String::new(),
            post_observed: None,
            phase: Phase::Ready,
            session: None,
            store: String::new(),
            initial: None,
            verified: None,
            journal_recorded: false,
            snapshot_saved: false,
            command_started: false,
            activity: VecDeque::new(),
            omitted: 0,
            error: None,
        }
    }
}

impl Report {
    fn record(&mut self, text: &str) {
        if self.activity.len() == MAX_ACTIVITY {
            self.activity.pop_front();
            self.omitted += 1;
        }
        self.activity.push_back(safe_text(text));
    }

    pub(super) fn cancel(&mut self) {
        if self.phase == Phase::Running {
            self.phase = Phase::Cancelling;
            self.record("! Cancellation requested. Waiting for bounded software cleanup.");
        }
    }

    pub(super) fn finish(&mut self, result: Result<i32, String>) {
        if let Err(error) = result {
            if self.phase != Phase::Interrupted {
                self.phase = Phase::Failed;
            }
            // Keep the detailed terminal event (for example disk-full) when
            // the runner subsequently returns a generic error summary.
            self.error.get_or_insert_with(|| safe_text(&error));
        }
    }

    fn event(&mut self, name: &str, data: Option<Value>) -> Result<(), String> {
        match name {
            "session_started" => {
                if self.phase != Phase::Cancelling {
                    self.phase = Phase::Running;
                }
                self.record("+ Session opened. Fresh simulator; no earlier motion replayed.");
            }
            "agent_event" => {
                let event: Event = serde_json::from_value(data.ok_or("missing agent event")?)
                    .map_err(|_| "invalid agent event")?;
                match event {
                    Event::ToolStarted { tool, .. } if tool == "robot_observe" => {
                        self.record(".. Observe requested. Waiting for the current joint state.");
                    }
                    Event::ToolStarted { tool, .. } if tool == "robot_command" => {
                        self.command_started = true;
                        self.record(
                            ".. Command tool started. Dispatch and outcome not yet confirmed.",
                        );
                    }
                    Event::ToolCompleted {
                        tool,
                        is_error: false,
                        metadata,
                        ..
                    } if tool == "robot_observe" => {
                        self.initial = finite(&metadata["joints"]["shoulder"]);
                        if let Some(value) = self.initial {
                            self.record(&format!(
                                "+ Initial observation: shoulder {value:.3} rad."
                            ));
                        }
                    }
                    Event::ToolCompleted {
                        tool,
                        is_error: false,
                        metadata,
                        ..
                    } if tool == "robot_command" => {
                        // The fixed demo's target is known. A model response or
                        // acknowledgement alone must never set this evidence.
                        let observed = finite(
                            &metadata["metadata"]["post_action_state"]["joints"]["shoulder"],
                        );
                        if metadata["accepted"] == true && !self.demo {
                            self.post_observed = observed;
                            self.record(&format!(
                                "Command result / inspect evidence: {}",
                                metadata
                            ));
                        } else if metadata["accepted"] == false {
                            self.record(&format!("Command rejected: {}", metadata));
                        }
                        if metadata["accepted"] == true && self.demo {
                            if let Some(value) = observed.filter(|v| (v - 0.2).abs() <= 1e-6) {
                                self.verified = Some(value);
                                // JournalTool only returns after the verified
                                // result has been appended durably.
                                self.journal_recorded = true;
                                self.record(&format!("+ Position verified: shoulder {value:.3} rad. Journal recorded."));
                            }
                        }
                    }
                    Event::ToolCompleted { is_error: true, .. } => {
                        self.record(
                            "! Tool reported an error. This is not evidence of verification.",
                        );
                    }
                    Event::RunCompleted { output, .. } => {
                        self.answer = safe_text(&output);
                        self.record("+ Model finished. Saving the terminal snapshot.")
                    }
                    Event::RunFailed { error, .. } => self.record(&format!("! {error}")),
                    Event::Reconciled { unknown, .. } if unknown > 0 => {
                        self.record("! Unresolved outcomes found. Motion must not be replayed.")
                    }
                    _ => {}
                }
            }
            "session_finished" => {
                let data = data.unwrap_or(Value::Null);
                match data["outcome"].as_str() {
                    Some("success") => {
                        self.snapshot_saved = true;
                        self.phase = Phase::Complete;
                        self.record("+ Snapshot saved. This run is available for inspection.");
                    }
                    Some("interrupted") => {
                        self.phase = Phase::Interrupted;
                        self.record("! Run interrupted. No automatic retry or replay.");
                    }
                    _ => {
                        self.phase = Phase::Failed;
                        self.record("! Run failed. No automatic retry or replay.");
                    }
                }
                if let Some(error) = data["error"].as_str() {
                    self.error = Some(safe_text(error));
                }
            }
            // Legacy NDJSON calls model completion "verified". Never use that
            // envelope as evidence of robot state in this view.
            _ => {}
        }
        Ok(())
    }
}

fn finite(value: &Value) -> Option<f64> {
    value.as_f64().filter(|v| v.is_finite())
}

pub(super) fn safe_text(text: &str) -> String {
    redact(text)
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(512)
        .collect()
}

pub(super) struct UiOutput(pub(super) Arc<Mutex<Report>>);

impl RunOutput for UiOutput {
    fn emit(
        &self,
        seq: &mut u64,
        sid: &str,
        name: &str,
        data: Option<Value>,
    ) -> Result<(), String> {
        let data = data.map(redact_value).transpose()?;
        let mut report = self.0.lock().map_err(|_| "terminal report lock failed")?;
        *seq += 1;
        report.session = Some(safe_text(sid));
        report.event(name, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_completion_and_acknowledgement_are_not_verification() {
        let mut report = Report::default();
        report
            .event("verified", Some(json!({"output":"verified"})))
            .unwrap();
        report
            .event(
                "agent_event",
                Some(json!({"type":"run_completed","session_id":"s","output":"verified"})),
            )
            .unwrap();
        report.event("agent_event", Some(json!({"type":"tool_completed","turn":1,"call_id":"c","tool":"robot_command","is_error":false,"metadata":{"accepted":true}}))).unwrap();
        assert_eq!(report.verified, None);
        assert!(!report.journal_recorded);
    }

    #[test]
    fn verified_position_and_failed_snapshot_stay_distinct() {
        let mut report = Report::default();
        report.event("agent_event", Some(json!({"type":"tool_completed","turn":1,"call_id":"c","tool":"robot_command","is_error":false,"metadata":{"accepted":true,"metadata":{"post_action_state":{"joints":{"shoulder":0.2}}}}}))).unwrap();
        report
            .event(
                "session_finished",
                Some(json!({"outcome":"failed","error":"snapshot write failed"})),
            )
            .unwrap();
        assert_eq!(report.verified, Some(0.2));
        assert!(report.journal_recorded);
        assert!(!report.snapshot_saved);
        assert_eq!(report.phase, Phase::Failed);
    }

    #[test]
    fn diagnostic_storage_is_bounded_and_control_sequences_are_removed() {
        let mut report = Report::default();
        for _ in 0..100 {
            report.record(&"\x1b\r\u{202e}x".repeat(1000));
        }
        assert_eq!(report.activity.len(), MAX_ACTIVITY);
        assert_eq!(report.omitted, 100 - MAX_ACTIVITY);
        assert_eq!(report.activity[0].chars().count(), 512);
        assert!(!report.activity[0].contains('\x1b'));
        assert!(!report.activity[0].contains('\u{202e}'));
    }

    #[test]
    fn cancellation_and_detailed_errors_survive_late_runner_events() {
        let mut report = Report {
            phase: Phase::Running,
            ..Report::default()
        };
        report.cancel();
        report.event("session_started", None).unwrap();
        assert_eq!(report.phase, Phase::Cancelling);
        report
            .event(
                "session_finished",
                Some(json!({"outcome":"failed","error":"disk full"})),
            )
            .unwrap();
        report.finish(Err("snapshot failed".into()));
        assert_eq!(report.error.as_deref(), Some("disk full"));
    }
}
