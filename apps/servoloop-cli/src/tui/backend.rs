//! Serialized, persistent runtime for the terminal. Nothing in this module prints.
use super::types::{BackendCommand, BackendEvent, ChatMessage, Role, Settings, ToolEntry};
use async_trait::async_trait;
use serde_json::{json, Value};
use servoloop_core::{
    AgentLoop, Event, Message, Model, ModelRequest, ModelResponse, Session, StopToken, Tool,
    ToolCall, ToolOutput, ToolRegistry,
};
use servoloop_robot::{JointLimit, JointLimitPolicy, RobotHarness, RobotState, RobotStatus};
use servoloop_store::{new_id, JournalRecord, SessionGuard, Store, SCHEMA_VERSION};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex as StdMutex,
    },
};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        Mutex,
    },
    task::JoinHandle,
};

pub struct RuntimeHandle {
    pub commands: UnboundedSender<BackendCommand>,
    pub events: UnboundedReceiver<BackendEvent>,
    pub task: JoinHandle<()>,
    stop: Arc<StdMutex<Option<StopToken>>>,
    cancel_requested: Arc<AtomicBool>,
}

impl RuntimeHandle {
    /// Cancellation bypasses the busy command queue. The worker awaits cleanup.
    pub fn cancel(&self) {
        let current = self.stop.lock().unwrap_or_else(|e| e.into_inner());
        self.cancel_requested.store(true, Ordering::SeqCst);
        if let Some(stop) = current.as_ref() {
            stop.stop();
        }
    }
}

pub fn spawn() -> RuntimeHandle {
    let (commands, mut incoming) = mpsc::unbounded_channel();
    let (out, events) = mpsc::unbounded_channel();
    let stop = Arc::new(StdMutex::new(None));
    let current_stop = stop.clone();
    let cancel_requested = Arc::new(AtomicBool::new(false));
    let pending_cancel = cancel_requested.clone();
    let task = tokio::spawn(async move {
        let mut active: Option<Active> = None;
        while let Some(command) = incoming.recv().await {
            match command {
                BackendCommand::Shutdown => break,
                BackendCommand::NewSession => {
                    active = None;
                    send(
                        &out,
                        BackendEvent::SessionReady {
                            id: String::new(),
                            history: vec![],
                            resumed: false,
                        },
                    );
                    send(
                        &out,
                        BackendEvent::Status("New session · no commands sent".into()),
                    );
                }
                BackendCommand::Resume { id, settings } => {
                    // Keep the previous session (and lease) if opening fails.
                    match Active::open(&settings, Some(&id)) {
                        Ok(next) => {
                            send_ready(&out, &next, true);
                            active = Some(next);
                        }
                        Err(message) => send(
                            &out,
                            BackendEvent::Error {
                                message,
                                blocked: active.as_ref().is_some_and(|a| a.blocked),
                            },
                        ),
                    }
                }
                BackendCommand::Submit { prompt, settings } => {
                    if prompt.trim().is_empty() {
                        pending_cancel.store(false, Ordering::SeqCst);
                        send(
                            &out,
                            BackendEvent::Error {
                                message: "Enter a prompt first.".into(),
                                blocked: active.as_ref().is_some_and(|a| a.blocked),
                            },
                        );
                        continue;
                    }
                    if active.is_none() {
                        match Active::open(&settings, None) {
                            Ok(next) => {
                                send_ready(&out, &next, false);
                                active = Some(next);
                            }
                            Err(message) => {
                                pending_cancel.store(false, Ordering::SeqCst);
                                send(
                                    &out,
                                    BackendEvent::Error {
                                        message,
                                        blocked: false,
                                    },
                                );
                                continue;
                            }
                        }
                    }
                    if let Some(active) = active.as_mut() {
                        active
                            .submit(prompt, &settings, &out, &current_stop, &pending_cancel)
                            .await;
                    }
                }
            }
        }
    });
    RuntimeHandle {
        commands,
        events,
        task,
        stop,
        cancel_requested,
    }
}

/// Installation and clearing share the same lock with cancel(), closing the
/// window between enqueueing a Submit and installing its fresh stop token.
struct RunCancellation<'a> {
    current: &'a StdMutex<Option<StopToken>>,
    requested: &'a AtomicBool,
    stop: StopToken,
}
impl<'a> RunCancellation<'a> {
    fn install(current: &'a StdMutex<Option<StopToken>>, requested: &'a AtomicBool) -> Self {
        let stop = StopToken::new();
        let mut slot = current.lock().unwrap_or_else(|e| e.into_inner());
        if requested.load(Ordering::SeqCst) {
            stop.stop();
        }
        *slot = Some(stop.clone());
        Self {
            current,
            requested,
            stop,
        }
    }
}
impl Drop for RunCancellation<'_> {
    fn drop(&mut self) {
        let mut slot = self.current.lock().unwrap_or_else(|e| e.into_inner());
        *slot = None;
        self.requested.store(false, Ordering::SeqCst);
    }
}

fn diagnostic(out: &UnboundedSender<BackendEvent>, text: String) {
    send(
        out,
        BackendEvent::Message(ChatMessage {
            role: Role::System,
            text,
        }),
    );
}

// Redact at the final UI boundary as well as before every disk write.
fn send(out: &UnboundedSender<BackendEvent>, mut event: BackendEvent) {
    let clean = |s: &mut String| *s = crate::redact(s);
    match &mut event {
        BackendEvent::SessionReady { id, history, .. } => {
            clean(id);
            for m in history {
                clean(&mut m.text);
            }
        }
        BackendEvent::Status(s) => clean(s),
        BackendEvent::Tool(t) => {
            clean(&mut t.id);
            clean(&mut t.name);
            clean(&mut t.status);
            clean(&mut t.details);
        }
        BackendEvent::Message(m) => clean(&mut m.text),
        BackendEvent::Finished { summary, .. } => clean(summary),
        BackendEvent::Error { message, .. } => clean(message),
    }
    let _ = out.send(event);
}

fn history(session: &Session) -> Vec<ChatMessage> {
    session
        .messages
        .iter()
        .map(|m| {
            let (role, text) = match m {
                Message::User { content } => (Role::User, content.to_text()),
                Message::Assistant { content, .. } => (Role::Assistant, content.clone()),
                Message::System { content } => (Role::System, content.clone()),
                Message::Tool {
                    name,
                    content,
                    is_error,
                    ..
                } => (
                    Role::System,
                    format!("Historical tool {name} (error={is_error}): {content}"),
                ),
                Message::ToolUnknown { name, reason, .. } => {
                    (Role::System, format!("UNKNOWN {name}: {reason}"))
                }
            };
            ChatMessage { role, text }
        })
        .filter(|m| !m.text.is_empty())
        .collect()
}

fn send_ready(out: &UnboundedSender<BackendEvent>, active: &Active, resumed: bool) {
    send(
        out,
        BackendEvent::SessionReady {
            id: active.session.id.clone(),
            history: history(&active.session),
            resumed,
        },
    );
    if resumed {
        for message in &active.session.messages {
            if let Message::Tool {
                call_id,
                name,
                content,
                is_error,
            } = message
            {
                let evidence = serde_json::from_str::<Value>(content).ok();
                let status = if *is_error {
                    "Historical error · not verified"
                } else if name == "robot_command"
                    && evidence
                        .as_ref()
                        .is_some_and(|v| verified_receipt(&v["metadata"]))
                {
                    "Historical verified receipt · not current state"
                } else if name == "robot_observe"
                    && evidence
                        .as_ref()
                        .is_some_and(|v| v.get("joints").is_some_and(Value::is_object))
                {
                    "Historical observation · not current state"
                } else {
                    "Historical result · evidence unavailable"
                };
                let details = evidence
                    .map(crate::redact_value)
                    .transpose()
                    .map(|v| v.map_or_else(|| crate::redact(content), |v| v.to_string()))
                    .unwrap_or_else(|_| "Historical evidence withheld: redaction failed".into());
                send(
                    out,
                    BackendEvent::Tool(ToolEntry {
                        id: call_id.clone(),
                        name: name.clone(),
                        status: status.into(),
                        details,
                        is_error: *is_error,
                    }),
                );
            }
        }
    }
    send(
        out,
        BackendEvent::Status(
            if resumed {
                "Resumed history · fresh simulator · idle"
            } else {
                "Simulated robot · ready"
            }
            .into(),
        ),
    );
}

struct Active {
    session: Session,
    store: Store,
    guard: Arc<SessionGuard>,
    harness: RobotHarness,
    blocked: bool,
    persistence_fault: Arc<AtomicBool>,
    evidence_notes: Arc<StdMutex<Vec<String>>>,
}

impl Active {
    fn open(settings: &Settings, resume: Option<&str>) -> Result<Self, String> {
        if resume.is_some() && !settings.store_path.is_dir() {
            return Err("Session store not found; resume did not create any directories.".into());
        }
        let store = Store::open(&settings.store_path).map_err(|e| e.to_string())?;
        if let Some(id) = resume {
            if !store
                .sessions()
                .map_err(|e| e.to_string())?
                .iter()
                .any(|s| s == id)
            {
                return Err("Session not found; resume did not create a session.".into());
            }
        }
        let id = resume
            .map(str::to_owned)
            .unwrap_or_else(|| new_id("session"));
        if resume.is_none() {
            store.create_session(&id).map_err(|e| e.to_string())?;
        }
        let guard = Arc::new(store.acquire_session(&id).map_err(|e| e.to_string())?);
        let mut session = if resume.is_some() {
            store
                .load_snapshot_guarded(&guard)
                .map_err(|e| e.to_string())?
        } else {
            Session::new(&id)
        };
        session
            .reconcile_tool_history()
            .map_err(|e| e.to_string())?;
        if session.has_unresolved_unknowns()
            || !store.unresolved(&id).map_err(|e| e.to_string())?.is_empty()
        {
            return Err(
                "Unresolved tool outcome; resume refused. This terminal cannot resolve unknown outcomes.".into(),
            );
        }
        if resume.is_some() {
            session.messages.push(Message::System { content: "Resume disclaimer: this is a fresh simulated environment. Prior observations and tool results are historical, not current state. Do not replay prior movement. No tools have run on resume.".into() });
        }
        let driver = Arc::new(crate::SimulatedDriver(Mutex::new(RobotState {
            joints: BTreeMap::from([("shoulder".into(), 0.0)]),
            battery_percent: Some(100.0),
            ..Default::default()
        })));
        let harness = RobotHarness::new(
            driver,
            Arc::new(JointLimitPolicy {
                limits: BTreeMap::from([(
                    "shoulder".into(),
                    JointLimit {
                        min: -1.5,
                        max: 1.5,
                        max_step: 0.25,
                    },
                )]),
                minimum_battery_percent: Some(10.0),
            }),
        );
        Ok(Self {
            session,
            store,
            guard,
            harness,
            blocked: false,
            persistence_fault: Arc::new(AtomicBool::new(false)),
            evidence_notes: Arc::new(StdMutex::new(vec![])),
        })
    }

    async fn gate(&mut self) -> bool {
        self.blocked |= self.session.reconcile_tool_history().is_err()
            || self.session.has_unresolved_unknowns()
            || self.persistence_fault.load(Ordering::SeqCst)
            || self
                .store
                .unresolved(&self.session.id)
                .map_or(true, |r| !r.is_empty())
            || !matches!(self.harness.status().await, RobotStatus::Ready);
        self.blocked
    }

    fn tools(
        &self,
        out: &UnboundedSender<BackendEvent>,
        dispatched: Arc<AtomicBool>,
    ) -> Result<ToolRegistry, String> {
        let mut raw = ToolRegistry::new();
        self.harness
            .register_tools(&mut raw)
            .map_err(|e| e.to_string())?;
        let mut tools = ToolRegistry::new();
        tools
            .register_arc(raw.get("robot_observe").ok_or("Missing observation tool")?)
            .map_err(|e| e.to_string())?;
        tools
            .register(SafeJournal {
                inner: raw.get("robot_command").ok_or("Missing command tool")?,
                guard: self.guard.clone(),
                sid: self.session.id.clone(),
                fault: self.persistence_fault.clone(),
                out: out.clone(),
                dispatched,
                evidence_notes: self.evidence_notes.clone(),
            })
            .map_err(|e| e.to_string())?;
        Ok(tools)
    }

    async fn submit(
        &mut self,
        prompt: String,
        settings: &Settings,
        out: &UnboundedSender<BackendEvent>,
        current_stop: &Arc<StdMutex<Option<StopToken>>>,
        cancel_requested: &AtomicBool,
    ) {
        let cancellation = RunCancellation::install(current_stop, cancel_requested);
        let stop = cancellation.stop.clone();
        if stop.is_stopped() {
            drop(cancellation);
            send(
                out,
                BackendEvent::Finished {
                    summary: "Cancelled before execution; no model or tools started.".into(),
                    blocked: self.blocked,
                    snapshot_saved: false,
                },
            );
            return;
        }
        if self.gate().await {
            drop(cancellation);
            send(out, BackendEvent::Error { message: "Session blocked: unknown outcome, persistence failure, or non-ready robot. No further execution is permitted in this session.".into(), blocked: true });
            return;
        }
        let dispatched = Arc::new(AtomicBool::new(false));
        let prepared = (|| -> Result<AgentLoop, String> {
            let model: Arc<dyn Model> = if settings.demo {
                Arc::new(ScriptedModel {
                    request: DemoRequest::parse(&prompt),
                    phase: AtomicUsize::new(0),
                })
            } else {
                let spec = crate::provider(
                    &settings.provider,
                    (!settings.base_url.trim().is_empty()).then(|| settings.base_url.clone()),
                )?;
                spec.validate().map_err(|e| e.to_string())?;
                Arc::new(
                    servoloop_providers::OpenAiCompatProvider::new(spec, &settings.model)
                        .map_err(|e| e.to_string())?,
                )
            };
            Ok(AgentLoop::new(model, self.tools(out, dispatched.clone())?, "Operate only the simulated robot conservatively. Observe before motion; only move_joint for shoulder is supported. Respect joint limits and a maximum step of 0.25 radians. Command tools verify internally. Never describe run completion as proof of motion; report actual tool evidence."))
        })();
        let agent = match prepared {
            Ok(agent) => agent,
            Err(message) => {
                drop(cancellation);
                send(
                    out,
                    BackendEvent::Error {
                        message,
                        blocked: false,
                    },
                );
                return;
            }
        };
        let event_fault = AtomicBool::new(false);
        let callback = |event| {
            if let Err(error) = forward_event(out, event) {
                event_fault.store(true, Ordering::SeqCst);
                stop.stop();
                diagnostic(
                    out,
                    format!("Event redaction failed; stopping execution: {error}"),
                );
            }
        };
        let result = agent.run(&mut self.session, prompt, &callback, &stop).await;
        let interrupted = stop.is_stopped();
        if interrupted && dispatched.load(Ordering::SeqCst) {
            if let Err(error) = self.harness.emergency_stop().await {
                self.blocked = true;
                diagnostic(out, format!("Stop cleanup failed: {error}"));
            }
        }
        for content in self
            .evidence_notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            self.session.messages.push(Message::System { content });
        }
        self.blocked |= event_fault.load(Ordering::SeqCst);
        self.gate().await;
        let saved = crate::redacted_session(&self.session)
            .and_then(|s| self.guard.save_snapshot(&s).map_err(|e| e.to_string()));
        let snapshot_saved = saved.is_ok();
        if let Err(error) = saved {
            self.blocked = true;
            diagnostic(
                out,
                format!("Snapshot not saved; history retained in memory: {error}"),
            );
        }
        let summary = match result {
            Ok(output) if !interrupted => {
                send(
                    out,
                    BackendEvent::Message(ChatMessage {
                        role: Role::Assistant,
                        text: output,
                    }),
                );
                "Run completed; see individual tool evidence for motion outcome.".into()
            }
            Ok(_) => "Cancelled; stop cleanup awaited. No completion claim.".into(),
            Err(error) => {
                diagnostic(out, error.to_string());
                if interrupted {
                    "Cancelled; stop cleanup awaited.".into()
                } else {
                    "Run failed; history retained in memory.".into()
                }
            }
        };
        drop(cancellation);
        send(
            out,
            BackendEvent::Finished {
                summary,
                blocked: self.blocked,
                snapshot_saved,
            },
        );
    }
}

fn forward_event(out: &UnboundedSender<BackendEvent>, event: Event) -> Result<(), String> {
    let details = crate::redact_value(serde_json::to_value(&event).map_err(|e| e.to_string())?)?;
    match event {
        Event::ToolStarted { call_id, tool, .. } => send(
            out,
            BackendEvent::Tool(ToolEntry {
                id: call_id,
                name: tool,
                status: "Started · dispatch not confirmed".into(),
                details: details.to_string(),
                is_error: false,
            }),
        ),
        Event::ToolCompleted {
            call_id,
            tool,
            is_error,
            metadata,
            ..
        } => {
            let status = if is_error {
                "Error · not verified"
            } else if tool == "robot_command" && verified_receipt(&metadata) {
                "Verified · post-action observation"
            } else if tool == "robot_observe" {
                "Observed"
            } else {
                "Completed · no motion verification evidence"
            };
            send(
                out,
                BackendEvent::Tool(ToolEntry {
                    id: call_id,
                    name: tool,
                    status: status.into(),
                    details: details.to_string(),
                    is_error,
                }),
            );
        }
        Event::RunStarted { .. } => send(
            out,
            BackendEvent::Status("Running · simulated robot".into()),
        ),
        Event::ModelAttempt { .. } => send(out, BackendEvent::Status("Waiting for model".into())),
        Event::ModelRetry { error, .. } => send(
            out,
            BackendEvent::Status(format!("Model attempt failed: {error}")),
        ),
        // RunCompleted is not a robot verification event. Final output is emitted
        // after stop cleanup and persistence have been attempted.
        _ => {}
    }
    Ok(())
}

/// Latches journal failures immediately, including between calls in one run.
struct SafeJournal {
    inner: Arc<dyn Tool>,
    guard: Arc<SessionGuard>,
    sid: String,
    fault: Arc<AtomicBool>,
    out: UnboundedSender<BackendEvent>,
    dispatched: Arc<AtomicBool>,
    evidence_notes: Arc<StdMutex<Vec<String>>>,
}

fn verified_receipt(metadata: &Value) -> bool {
    metadata.get("accepted").and_then(Value::as_bool) == Some(true)
        && metadata
            .pointer("/metadata/post_action_state/joints/shoulder")
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
}
#[async_trait]
impl Tool for SafeJournal {
    fn definition(&self) -> servoloop_core::ToolDefinition {
        let mut definition = self.inner.definition();
        definition.description =
            "Move only the simulated shoulder, with internal safety checks and verification."
                .into();
        definition.parameters = json!({"type":"object","properties":{"command":{"const":"move_joint"},"joint":{"const":"shoulder"},"position":{"type":"number"}},"required":["command","joint","position"],"additionalProperties":false});
        definition
    }
    async fn execute(&self, args: Value) -> servoloop_core::Result<ToolOutput> {
        use servoloop_core::Error;
        if self.fault.load(Ordering::SeqCst) {
            return Err(Error::Tool("Session journal blocked; no dispatch".into()));
        }
        if args.get("command").and_then(Value::as_str) != Some("move_joint")
            || args.get("joint").and_then(Value::as_str) != Some("shoulder")
            || !args
                .get("position")
                .and_then(Value::as_f64)
                .is_some_and(f64::is_finite)
            || args.as_object().is_none_or(|o| o.len() != 3)
        {
            return Err(Error::InvalidInput("Unsupported simulated command; only move_joint for shoulder is implemented. No dispatch.".into()));
        }
        let mut record = JournalRecord {
            version: SCHEMA_VERSION,
            sequence: 0,
            session_id: self.sid.clone(),
            intent_id: new_id("intent"),
            kind: "intent".into(),
            arguments: crate::redact_value(args.clone()).map_err(|e| {
                self.fault.store(true, Ordering::SeqCst);
                Error::Tool(e)
            })?,
            outcome: None,
        };
        if let Err(error) = self.guard.append(record.clone()) {
            self.fault.store(true, Ordering::SeqCst);
            return Err(Error::Tool(format!("Journal failed; no dispatch: {error}")));
        }
        self.dispatched.store(true, Ordering::SeqCst);
        let result = self.inner.execute(args).await;
        record.kind = "result".into();
        match &result {
            Ok(output) => {
                if verified_receipt(&output.metadata) {
                    record.outcome = Some("verified".into());
                } else {
                    // The store only accepts verified terminal outcomes. A
                    // receipt without evidence must remain unresolved.
                    self.fault.store(true, Ordering::SeqCst);
                }
            }
            Err(_) => {
                self.fault.store(true, Ordering::SeqCst);
            }
        }
        if let Err(error) = self.guard.append(record) {
            self.fault.store(true, Ordering::SeqCst);
            if let Ok(output) = &result {
                let receipt = crate::redact_value(
                    json!({"content": output.content, "metadata": output.metadata}),
                )
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "Evidence withheld: redaction failed".into());
                let text = format!("Actual robot receipt (not durably journaled): {receipt}. Persistence failure: {error}. Session blocked; this evidence does not resolve the journal.");
                self.evidence_notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(text.clone());
                diagnostic(&self.out, text);
            }
            return Err(Error::Tool(format!(
                "Outcome journal write failed: {error}"
            )));
        }
        // Keep actual receipt evidence in Message::Tool without changing the
        // core/store schema. Resume can distinguish it from old plain results.
        result.and_then(|mut output| {
            output.content = crate::redact_value(
                json!({"content": output.content, "metadata": output.metadata}),
            )
            .map_err(|error| {
                self.fault.store(true, Ordering::SeqCst);
                Error::Tool(error)
            })?
            .to_string();
            Ok(output)
        })
    }
}

#[derive(Clone, Copy)]
enum DemoRequest {
    Inspect,
    Limits,
    Move(f64),
    Unsupported,
}
impl DemoRequest {
    fn parse(prompt: &str) -> Self {
        let normalized = prompt.trim().trim_end_matches('.').to_ascii_lowercase();
        match normalized.as_str() {
            "inspect"
            | "inspect positions"
            | "show positions"
            | "show joint positions"
            | "inspect the current joint positions"
            | "observe" => Self::Inspect,
            "limits"
            | "explain limits"
            | "explain safety limits"
            | "explain the execution limits" => Self::Limits,
            _ => {
                let value = normalized
                    .strip_prefix("move shoulder to ")
                    .or_else(|| normalized.strip_prefix("move the shoulder to "));
                value
                    .and_then(|v| v.strip_suffix(" radians").unwrap_or(v).parse::<f64>().ok())
                    .filter(|p| p.is_finite() && (-1.5..=1.5).contains(p))
                    .map(Self::Move)
                    .unwrap_or(Self::Unsupported)
            }
        }
    }
}

struct ScriptedModel {
    request: DemoRequest,
    phase: AtomicUsize,
}
#[async_trait]
impl Model for ScriptedModel {
    async fn complete(&self, request: ModelRequest) -> servoloop_core::Result<ModelResponse> {
        let phase = self.phase.fetch_add(1, Ordering::SeqCst);
        let call = |name: &str, arguments| ModelResponse {
            tool_calls: vec![ToolCall {
                id: new_id("demo-call"),
                name: name.into(),
                arguments,
            }],
            ..Default::default()
        };
        let last_tool = request.messages.iter().rev().find_map(|m| match m {
            Message::Tool {
                content, is_error, ..
            } => Some((content, *is_error)),
            _ => None,
        });
        Ok(match self.request {
            DemoRequest::Unsupported => ModelResponse::text("Offline demo supports: inspect positions; explain limits; move shoulder to <radians>. Use a finite target within [-1.5, 1.5], at most 0.25 radians from the current position. No motion requested for unrecognized syntax."),
            DemoRequest::Limits => ModelResponse::text("Simulated shoulder limits: -1.5 to 1.5 radians; maximum step 0.25 radians; minimum battery 10%. Commands observe, check policy, execute, and verify internally. No motion performed."),
            DemoRequest::Inspect | DemoRequest::Move(_) if phase == 0 => call("robot_observe", json!({})),
            DemoRequest::Move(target) if phase == 1 => {
                let position = last_tool.filter(|(_, error)| !error).and_then(|(s, _)| serde_json::from_str::<Value>(s).ok()).and_then(|v| v.pointer("/joints/shoulder").and_then(Value::as_f64));
                if position.is_some_and(|p| (target - p).abs() <= 0.25) {
                    call("robot_command", json!({"command":"move_joint", "joint":"shoulder", "position":target}))
                } else { ModelResponse::text("No motion performed: requested step exceeds 0.25 radians or current observation is unavailable.") }
            }
            _ => ModelResponse::text(match last_tool {
                Some((content, true)) => format!("Tool failed; no verified motion claim. {content}"),
                Some((content, false)) => demo_summary(self.request, content),
                None => "No tool result is available; no verified motion claim.".into(),
            }),
        })
    }
}

fn demo_summary(request: DemoRequest, content: &str) -> String {
    let Ok(evidence) = serde_json::from_str::<Value>(content) else {
        return "Tool result received, but structured evidence is unavailable. No verified motion claim.".into();
    };
    match request {
        DemoRequest::Move(target) if verified_receipt(&evidence["metadata"]) => {
            match evidence.pointer("/metadata/metadata/post_action_state/joints/shoulder").and_then(Value::as_f64) {
                Some(observed) if observed.is_finite() && (observed - target).abs() <= 1e-6 => format!("Shoulder position verified: target {target:.3} rad, observed {observed:.3} rad, error {:.3} rad.", (observed - target).abs()),
                Some(observed) if observed.is_finite() => format!("Shoulder observation {observed:.3} rad does not match target {target:.3} rad. Requested position not verified."),
                _ => "Receipt received, but shoulder observation is unavailable. Requested position not verified.".into(),
            }
        }
        DemoRequest::Inspect => {
            let shoulder = evidence.pointer("/joints/shoulder").and_then(Value::as_f64).filter(|v| v.is_finite()).map(|v| format!("{v:.3} rad")).unwrap_or_else(|| "unavailable".into());
            let battery = evidence.get("battery_percent").and_then(Value::as_f64).filter(|v| v.is_finite()).map(|v| format!("{v:.1}%")).unwrap_or_else(|| "unavailable".into());
            format!("Simulated shoulder position: {shoulder}. Battery: {battery}. No motion performed.")
        }
        _ => "Tool result received, but post-action verification evidence is unavailable. No verified motion claim.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> Settings {
        Settings {
            demo: true,
            provider: "ollama".into(),
            model: "test".into(),
            base_url: String::new(),
            store_path: std::env::temp_dir().join(new_id("tui-backend-test")),
            config_path: "unused".into(),
        }
    }
    async fn turn(active: &mut Active, prompt: &str, settings: &Settings) -> Vec<BackendEvent> {
        let (out, mut events) = mpsc::unbounded_channel();
        active
            .submit(
                prompt.into(),
                settings,
                &out,
                &Arc::new(StdMutex::new(None)),
                &AtomicBool::new(false),
            )
            .await;
        drop(out);
        let mut collected = vec![];
        while let Some(event) = events.recv().await {
            collected.push(event);
        }
        collected
    }
    async fn position(active: &Active) -> f64 {
        let mut tools = ToolRegistry::new();
        active.harness.register_tools(&mut tools).unwrap();
        let output = tools
            .get("robot_observe")
            .unwrap()
            .execute(json!({}))
            .await
            .unwrap();
        output.metadata["joints"]["shoulder"].as_f64().unwrap()
    }
    #[tokio::test]
    async fn no_motion_until_submit() {
        let settings = settings();
        let handle = spawn();
        handle.commands.send(BackendCommand::NewSession).unwrap();
        handle.commands.send(BackendCommand::Shutdown).unwrap();
        handle.task.await.unwrap();
        assert!(!settings.store_path.exists());
        let active = Active::open(&settings, None).unwrap();
        assert!(active.store.records(&active.session.id).unwrap().is_empty());
        assert_eq!(position(&active).await, 0.0);
    }
    #[tokio::test]
    async fn sequential_moves_keep_same_simulator_and_history() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        turn(&mut active, "move shoulder to 0.2", &settings).await;
        let events = turn(&mut active, "move shoulder to 0.4", &settings).await;
        assert_eq!(position(&active).await, 0.4);
        assert!(!active.blocked);
        assert_eq!(
            active
                .session
                .messages
                .iter()
                .filter(|m| matches!(m, Message::User { .. }))
                .count(),
            2
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, BackendEvent::Tool(t) if t.status.starts_with("Verified"))));
        assert!(events.iter().any(|e| matches!(
            e,
            BackendEvent::Finished {
                snapshot_saved: true,
                blocked: false,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(e, BackendEvent::Message(m) if m.role == Role::Assistant && m.text == "Shoulder position verified: target 0.400 rad, observed 0.400 rad, error 0.000 rad.")));
        assert!(active.session.messages.iter().any(|m| matches!(m, Message::Tool { name, content, .. } if name == "robot_command" && content.contains("post_action_state"))));
    }
    #[tokio::test]
    async fn resume_is_idle_and_preserves_lease() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        turn(&mut active, "move shoulder to 0.2", &settings).await;
        let id = active.session.id.clone();
        let count = active.store.records(&id).unwrap().len();
        drop(active);
        let mut handle = spawn();
        handle
            .commands
            .send(BackendCommand::Resume {
                id: id.clone(),
                settings: settings.clone(),
            })
            .unwrap();
        let event = handle.events.recv().await.unwrap();
        assert!(
            matches!(event, BackendEvent::SessionReady { resumed: true, history, .. } if history.iter().any(|m| m.text.contains("Resume disclaimer")))
        );
        let store = Store::open(&settings.store_path).unwrap();
        assert_eq!(store.records(&id).unwrap().len(), count);
        assert!(store.acquire_session(&id).is_err());
        handle.commands.send(BackendCommand::Shutdown).unwrap();
        handle.task.await.unwrap();
        let mut historical_command = false;
        while let Ok(event) = handle.events.try_recv() {
            if let BackendEvent::Tool(tool) = event {
                assert!(tool.status.starts_with("Historical"));
                if tool.name == "robot_command" {
                    historical_command = true;
                    assert!(tool.status.contains("verified receipt"));
                    let evidence: Value = serde_json::from_str(&tool.details).unwrap();
                    assert_eq!(
                        evidence.pointer("/metadata/metadata/post_action_state/joints/shoulder"),
                        Some(&json!(0.2))
                    );
                }
            }
        }
        assert!(historical_command);
        let resumed = Active::open(&settings, Some(&id)).unwrap();
        assert_eq!(position(&resumed).await, 0.0);
    }
    #[tokio::test]
    async fn unknown_blocks_future_sends() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        active.session.messages.push(Message::ToolUnknown {
            call_id: "uncertain".into(),
            name: "robot_command".into(),
            reason: "interrupted".into(),
        });
        let events = turn(&mut active, "move shoulder to 0.2", &settings).await;
        assert!(active.blocked);
        assert_eq!(position(&active).await, 0.0);
        assert!(events
            .iter()
            .any(|e| matches!(e, BackendEvent::Error { blocked: true, .. })));
        assert!(!events.iter().any(|e| matches!(e, BackendEvent::Tool(_))));
    }
    #[tokio::test]
    async fn invalid_demo_requests_never_move() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        for prompt in [
            "dance",
            "move shoulder to NaN",
            "move shoulder to inf",
            "move shoulder to 2",
            "move shoulder to 0.2 then 0.4",
            "move shoulder to 0.8",
        ] {
            let events = turn(&mut active, prompt, &settings).await;
            assert!(!events
                .iter()
                .any(|e| matches!(e, BackendEvent::Tool(t) if t.name == "robot_command")));
            assert_eq!(position(&active).await, 0.0);
        }
    }
    #[tokio::test]
    async fn journal_unknown_also_blocks() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        active
            .guard
            .append(JournalRecord {
                version: SCHEMA_VERSION,
                sequence: 0,
                session_id: active.session.id.clone(),
                intent_id: new_id("intent"),
                kind: "intent".into(),
                arguments: json!({}),
                outcome: None,
            })
            .unwrap();
        turn(&mut active, "move shoulder to 0.2", &settings).await;
        assert!(active.blocked);
        assert_eq!(position(&active).await, 0.0);
    }

    #[tokio::test]
    async fn exact_suggestion_prompts_work() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        let inspect = turn(
            &mut active,
            "Inspect the current joint positions.",
            &settings,
        )
        .await;
        assert!(inspect.iter().any(|e| matches!(e, BackendEvent::Message(m) if m.role == Role::Assistant && m.text == "Simulated shoulder position: 0.000 rad. Battery: 100.0%. No motion performed.")));
        assert!(inspect.iter().any(|e| matches!(e, BackendEvent::Tool(t) if t.name == "robot_observe" && t.status == "Observed")));
        let limits = turn(&mut active, "Explain the execution limits.", &settings).await;
        assert!(limits.iter().any(|e| matches!(e, BackendEvent::Message(m) if m.role == Role::Assistant && m.text.contains("maximum step 0.25"))));
        assert!(!limits.iter().any(|e| matches!(e, BackendEvent::Tool(_))));
        assert_eq!(position(&active).await, 0.0);
    }

    async fn terminal(handle: &mut RuntimeHandle) -> Vec<BackendEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            let mut events = vec![];
            loop {
                let event = handle
                    .events
                    .recv()
                    .await
                    .expect("runtime closed before terminal event");
                let done = matches!(
                    event,
                    BackendEvent::Finished { .. } | BackendEvent::Error { .. }
                );
                events.push(event);
                if done {
                    return events;
                }
            }
        })
        .await
        .expect("runtime did not finish")
    }

    #[tokio::test]
    async fn immediate_cancel_is_latched_then_cleared_for_next_run() {
        let settings = settings();
        let mut handle = spawn();
        handle
            .commands
            .send(BackendCommand::Submit {
                prompt: "move shoulder to 0.2".into(),
                settings: settings.clone(),
            })
            .unwrap();
        // No yield: the worker has not installed a stop token yet.
        handle.cancel();
        let events = terminal(&mut handle).await;
        assert!(!events.iter().any(|e| matches!(e, BackendEvent::Tool(_))));
        assert!(events.iter().any(|e| matches!(e, BackendEvent::Finished { summary, blocked: false, .. } if summary.contains("before execution"))));
        assert!(!handle.cancel_requested.load(Ordering::SeqCst));
        handle
            .commands
            .send(BackendCommand::Submit {
                prompt: "move shoulder to 0.2".into(),
                settings,
            })
            .unwrap();
        let events = terminal(&mut handle).await;
        assert!(events
            .iter()
            .any(|e| matches!(e, BackendEvent::Tool(t) if t.status.starts_with("Verified"))));
        handle.commands.send(BackendCommand::Shutdown).unwrap();
        handle.task.await.unwrap();
    }

    #[tokio::test]
    async fn stopped_harness_blocks_without_dispatch() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        active.harness.emergency_stop().await.unwrap();
        assert_eq!(active.harness.status().await, RobotStatus::StopAcknowledged);
        let events = turn(&mut active, "move shoulder to 0.2", &settings).await;
        assert!(active.blocked);
        assert!(!events.iter().any(|e| matches!(e, BackendEvent::Tool(_))));
        assert!(active.store.records(&active.session.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn nonexistent_resume_creates_no_session_directory() {
        let settings = settings();
        assert!(Active::open(&settings, Some("missing-session")).is_err());
        assert!(!settings.store_path.exists());
        assert!(!settings.store_path.join("missing-session").exists());
        assert!(Store::open(&settings.store_path)
            .unwrap()
            .sessions()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn old_snapshot_results_do_not_invent_verification() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        active.session.messages.push(Message::Tool {
            call_id: "legacy".into(),
            name: "robot_command".into(),
            content: "accepted".into(),
            is_error: false,
        });
        let (out, mut events) = mpsc::unbounded_channel();
        send_ready(&out, &active, true);
        assert!(matches!(
            events.recv().await,
            Some(BackendEvent::SessionReady { .. })
        ));
        assert!(
            matches!(events.recv().await, Some(BackendEvent::Tool(t)) if t.status == "Historical result · evidence unavailable")
        );
    }

    #[tokio::test]
    async fn demo_summary_requires_actual_matching_evidence() {
        for content in [
            "accepted",
            "{}",
            r#"{"metadata":{"accepted":true,"metadata":{"post_action_state":{"joints":{}}}}}"#,
        ] {
            assert!(demo_summary(DemoRequest::Move(0.2), content).contains("unavailable"));
        }
        let evidence = json!({"metadata":{"accepted":true,"metadata":{"post_action_state":{"joints":{"shoulder":0.1}}}}}).to_string();
        assert!(demo_summary(DemoRequest::Move(0.2), &evidence).contains("does not match target"));
        assert_eq!(
            demo_summary(DemoRequest::Inspect, "{}"),
            "Simulated shoulder position: unavailable. Battery: unavailable. No motion performed."
        );
    }

    #[tokio::test]
    async fn unsupported_simulator_commands_never_reach_journal_or_driver() {
        let settings = settings();
        let active = Active::open(&settings, None).unwrap();
        let (out, _) = mpsc::unbounded_channel();
        let dispatched = Arc::new(AtomicBool::new(false));
        let command = active
            .tools(&out, dispatched.clone())
            .unwrap()
            .get("robot_command")
            .unwrap();
        for args in [
            json!({"command":"move_joints","positions":{"shoulder":0.2}}),
            json!({"command":"set_output","channel":"grip","value":1}),
            json!({"command":"stop"}),
            json!({"command":"move_joint","joint":"elbow","position":0.2}),
        ] {
            assert!(command.execute(args).await.is_err());
        }
        assert!(!dispatched.load(Ordering::SeqCst));
        assert!(active.store.records(&active.session.id).unwrap().is_empty());
        assert_eq!(position(&active).await, 0.0);
    }

    struct CorruptJournalAfterReceipt {
        inner: Arc<dyn Tool>,
        path: std::path::PathBuf,
    }
    #[async_trait]
    impl Tool for CorruptJournalAfterReceipt {
        fn definition(&self) -> servoloop_core::ToolDefinition {
            self.inner.definition()
        }
        async fn execute(&self, args: Value) -> servoloop_core::Result<ToolOutput> {
            let receipt = self.inner.execute(args).await?;
            // This test owns the entire temporary store. Simulate result-write
            // failure only after the real harness has returned its evidence.
            std::fs::write(&self.path, b"corrupt-test-journal\n").unwrap();
            Ok(receipt)
        }
    }

    #[tokio::test]
    async fn result_write_failure_keeps_actual_receipt_and_no_early_error_event() {
        let settings = settings();
        let active = Active::open(&settings, None).unwrap();
        let mut raw = ToolRegistry::new();
        active.harness.register_tools(&mut raw).unwrap();
        let (out, mut events) = mpsc::unbounded_channel();
        let command = SafeJournal {
            inner: Arc::new(CorruptJournalAfterReceipt {
                inner: raw.get("robot_command").unwrap(),
                path: settings
                    .store_path
                    .join(&active.session.id)
                    .join("journal.ndjson"),
            }),
            guard: active.guard.clone(),
            sid: active.session.id.clone(),
            fault: active.persistence_fault.clone(),
            out,
            dispatched: Arc::new(AtomicBool::new(false)),
            evidence_notes: active.evidence_notes.clone(),
        };
        let result = command
            .execute(json!({"command":"move_joint","joint":"shoulder","position":0.2}))
            .await;
        assert!(result.is_err());
        assert_eq!(position(&active).await, 0.2);
        assert!(active.persistence_fault.load(Ordering::SeqCst));
        let event = events.recv().await.unwrap();
        assert!(
            matches!(event, BackendEvent::Message(m) if m.role == Role::System && m.text.contains("post_action_state") && m.text.contains("0.2") && m.text.contains("Persistence failure"))
        );
        assert_eq!(active.evidence_notes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn snapshot_failure_only_finishes_after_diagnostics() {
        let settings = settings();
        let mut active = Active::open(&settings, None).unwrap();
        // A directory at the snapshot destination prevents atomic rename.
        std::fs::create_dir(
            settings
                .store_path
                .join(&active.session.id)
                .join("snapshot.json"),
        )
        .unwrap();
        let events = turn(&mut active, "explain limits", &settings).await;
        assert!(!events
            .iter()
            .any(|e| matches!(e, BackendEvent::Error { .. })));
        assert!(matches!(
            events.last(),
            Some(BackendEvent::Finished {
                blocked: true,
                snapshot_saved: false,
                ..
            })
        ));
        assert!(events.iter().any(
            |e| matches!(e, BackendEvent::Message(m) if m.text.contains("Snapshot not saved"))
        ));
    }

    /// Real loopback HTTP, explicit Ollama URL, no API keys/environment setup.
    /// Each connection is closed so retry counts correspond to actual requests.
    struct HttpFixture {
        url: String,
        requests: UnboundedReceiver<usize>,
        task: JoinHandle<()>,
    }
    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl HttpFixture {
        async fn start(responses: Vec<(u16, String)>, hang: bool) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let (out, requests) = mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                let mut count = 0;
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut bytes = vec![];
                    let mut chunk = [0; 4096];
                    loop {
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                break;
                            }
                        }
                        assert!(bytes.len() < 1024 * 1024);
                    }
                    assert!(
                        String::from_utf8_lossy(&bytes).starts_with("POST /v1/chat/completions ")
                    );
                    count += 1;
                    let _ = out.send(count);
                    if hang {
                        std::future::pending::<()>().await;
                    }
                    let (status, body) = responses
                        .get(count - 1)
                        .unwrap_or_else(|| responses.last().unwrap());
                    let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            });
            Self {
                url,
                requests,
                task,
            }
        }
    }

    #[tokio::test]
    async fn real_provider_retries_predispatch_failure_then_returns_actual_output() {
        let body = json!({"choices":[{"message":{"role":"assistant","content":"local provider final"},"finish_reason":"stop"}]}).to_string();
        let mut server = HttpFixture::start(
            vec![
                (503, "{\"error\":\"temporarily unavailable\"}".into()),
                (200, body),
            ],
            false,
        )
        .await;
        let mut settings = settings();
        settings.demo = false;
        settings.base_url = server.url.clone();
        let mut active = Active::open(&settings, None).unwrap();
        let events = turn(&mut active, "inspect", &settings).await;
        assert_eq!(server.requests.recv().await, Some(1));
        assert_eq!(server.requests.recv().await, Some(2));
        assert!(server.requests.try_recv().is_err());
        assert!(events.iter().any(
            |e| matches!(e, BackendEvent::Status(s) if s.starts_with("Model attempt failed"))
        ));
        assert!(events.iter().any(|e| matches!(e, BackendEvent::Message(m) if m.role == Role::Assistant && m.text == "local provider final")));
        assert!(!events
            .iter()
            .any(|e| matches!(e, BackendEvent::Tool(_) | BackendEvent::Error { .. })));
        assert!(active.store.records(&active.session.id).unwrap().is_empty());
        assert_eq!(active.harness.status().await, RobotStatus::Ready);
    }

    #[tokio::test]
    async fn real_provider_final_failure_does_not_claim_retry_or_finish_early() {
        let mut server =
            HttpFixture::start(vec![(400, "{\"error\":\"invalid model\"}".into())], false).await;
        let mut settings = settings();
        settings.demo = false;
        settings.base_url = server.url.clone();
        let mut active = Active::open(&settings, None).unwrap();
        let events = turn(&mut active, "inspect", &settings).await;
        assert_eq!(server.requests.recv().await, Some(1));
        assert!(server.requests.try_recv().is_err());
        assert!(!events
            .iter()
            .any(|e| matches!(e, BackendEvent::Tool(_) | BackendEvent::Error { .. })));
        assert!(events.iter().any(
            |e| matches!(e, BackendEvent::Status(s) if s.starts_with("Model attempt failed"))
        ));
        assert!(matches!(
            events.last(),
            Some(BackendEvent::Finished {
                blocked: false,
                snapshot_saved: true,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn real_provider_cancellation_before_tools_keeps_simulator_ready() {
        let mut server = HttpFixture::start(vec![], true).await;
        let mut settings = settings();
        settings.demo = false;
        settings.base_url = server.url.clone();
        let mut handle = spawn();
        handle
            .commands
            .send(BackendCommand::Submit {
                prompt: "move shoulder to 0.2".into(),
                settings: settings.clone(),
            })
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), server.requests.recv())
                .await
                .unwrap(),
            Some(1)
        );
        handle.cancel();
        let events = terminal(&mut handle).await;
        assert!(!events
            .iter()
            .any(|e| matches!(e, BackendEvent::Tool(_) | BackendEvent::Error { .. })));
        assert!(matches!(
            events.last(),
            Some(BackendEvent::Finished { blocked: false, .. })
        ));
        settings.demo = true;
        handle
            .commands
            .send(BackendCommand::Submit {
                prompt: "move shoulder to 0.2".into(),
                settings,
            })
            .unwrap();
        let events = terminal(&mut handle).await;
        assert!(events
            .iter()
            .any(|e| matches!(e, BackendEvent::Tool(t) if t.status.starts_with("Verified"))));
        handle.commands.send(BackendCommand::Shutdown).unwrap();
        handle.task.await.unwrap();
    }
}
