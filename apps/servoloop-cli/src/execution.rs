use crate::{
    args::{has, machine_output, prompt_from_args, value},
    commands::{configured_provider_with_key, setting},
    config::{store, Config},
    journal_tool::JournalTool,
    output::{redact_value, redacted_session, NdjsonOutput, RunOutput},
    simulation::{harness, DemoModel},
};
use serde_json::json;
use servoloop_core::{AgentLoop, Event as AgentEvent, LoopConfig, Model, StopToken, ToolRegistry};
use servoloop_providers::{OpenAiCompatProvider, Secret};
use servoloop_store::{new_id, JournalRecord, SessionGuard, SCHEMA_VERSION};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

pub(crate) const DEMO_PROMPT: &str = "Move the shoulder to 0.2 radians.";

struct RunControl {
    output: Arc<dyn RunOutput>,
    stop: StopToken,
    listen_for_signal: bool,
    provider_key: Option<Secret>,
}

impl RunControl {
    fn cli(args: &[String]) -> Self {
        Self {
            output: Arc::new(NdjsonOutput {
                quiet: machine_output(args),
            }),
            stop: StopToken::new(),
            listen_for_signal: true,
            provider_key: None,
        }
    }
}

/// The terminal view supplies presentation and cancellation, not another
/// execution engine. The demo always uses the existing scripted model and
/// fresh simulator, irrespective of configured provider credentials.
pub(crate) async fn interactive_demo(
    args: &[String],
    cfg: &Config,
    output: Arc<dyn RunOutput>,
    stop: StopToken,
) -> Result<i32, String> {
    run_loop(
        args,
        cfg,
        Arc::new(DemoModel(Mutex::new(0))),
        DEMO_PROMPT.into(),
        true,
        None,
        RunControl {
            output,
            stop,
            listen_for_signal: false,
            provider_key: None,
        },
    )
    .await
}

pub(crate) async fn run(args: &[String], cfg: &Config) -> Result<i32, String> {
    ensure_driver(args, cfg)?;
    let demo = has(args, "--demo");
    let prompt = if demo {
        DEMO_PROMPT.into()
    } else {
        prompt_from_args(args)?
    };
    run_loop(
        args,
        cfg,
        model(args, cfg, demo)?,
        prompt,
        demo,
        None,
        RunControl::cli(args),
    )
    .await
}

fn ensure_driver(args: &[String], cfg: &Config) -> Result<(), String> {
    if value(args, "--driver")
        .or_else(|| cfg.driver.clone())
        .as_deref()
        .unwrap_or("simulated")
        != "simulated"
    {
        return Err("only the simulated driver is implemented; Isaac is not available".into());
    }
    Ok(())
}

fn model(args: &[String], cfg: &Config, demo: bool) -> Result<Arc<dyn Model>, String> {
    model_with_key(args, cfg, demo, None)
}

fn model_with_key(
    args: &[String],
    cfg: &Config,
    demo: bool,
    key: Option<Secret>,
) -> Result<Arc<dyn Model>, String> {
    if demo {
        return Ok(Arc::new(DemoModel(Mutex::new(0))));
    }
    let name = setting(
        args,
        "--provider",
        "SERVOLOOP_PROVIDER",
        cfg.provider.clone(),
    )
    .ok_or("--provider is required")?;
    let model_name = setting(args, "--model", "SERVOLOOP_MODEL", cfg.model.clone())
        .ok_or("--model is required")?;
    let spec = configured_provider_with_key(
        &name,
        setting(
            args,
            "--base-url",
            "SERVOLOOP_BASE_URL",
            cfg.base_url.clone(),
        ),
        cfg,
        key,
    )?;
    spec.validate().map_err(|e| format!("provider: {e}"))?;
    Ok(Arc::new(
        OpenAiCompatProvider::new(spec, model_name).map_err(|e| e.to_string())?,
    ))
}

pub(crate) async fn resume(args: &[String], cfg: &Config) -> Result<i32, String> {
    resume_controlled(args, cfg, RunControl::cli(args)).await
}

/// Provider-backed terminal requests retain the same preflight, lease, tool,
/// journal and cleanup code as the scriptable commands.
pub(crate) async fn interactive_request(
    args: &[String],
    cfg: &Config,
    output: Arc<dyn RunOutput>,
    stop: StopToken,
    key: Option<Secret>,
) -> Result<i32, String> {
    let control = RunControl {
        output,
        stop,
        listen_for_signal: false,
        provider_key: key.clone(),
    };
    if args.first().map(String::as_str) == Some("resume") {
        return resume_controlled(args, cfg, control).await;
    }
    ensure_driver(args, cfg)?;
    run_loop(
        args,
        cfg,
        model_with_key(args, cfg, false, key)?,
        prompt_from_args(args)?,
        false,
        None,
        control,
    )
    .await
}

async fn resume_controlled(
    args: &[String],
    cfg: &Config,
    control: RunControl,
) -> Result<i32, String> {
    ensure_driver(args, cfg)?;
    let sid = args.get(1).ok_or("session ID is required")?.clone();
    let st = store(args, Some(cfg))?;
    if !st.sessions().map_err(|e| e.to_string())?.contains(&sid) {
        return Err("session not found".into());
    }
    // Acquire before loading either the journal or snapshot. The guard remains
    // alive through the complete run and terminal snapshot write.
    let guard = Arc::new(st.acquire_session(&sid).map_err(|e| e.to_string())?);
    let session = st
        .load_snapshot_guarded(&guard)
        .map_err(|e| format!("cannot resume session: {e}"))?;
    if !st.unresolved(&sid).map_err(|e| e.to_string())?.is_empty() {
        return Err("session has unresolved tool outcomes; refusing to resume".into());
    }
    let prompt = prompt_from_args(args)?;
    let model = model_with_key(args, cfg, has(args, "--demo"), control.provider_key.clone())?;
    run_loop(
        args,
        cfg,
        model,
        prompt,
        has(args, "--demo"),
        Some((sid, guard, session)),
        control,
    )
    .await
}

async fn run_loop(
    args: &[String],
    cfg: &Config,
    model: Arc<dyn Model>,
    prompt: String,
    simulated: bool,
    restored: Option<(String, Arc<SessionGuard>, servoloop_core::Session)>,
    control: RunControl,
) -> Result<i32, String> {
    control.output.starting(simulated);
    let st = store(args, Some(cfg))?;
    let is_restored = restored.is_some();
    let (sid, guard, mut session) = match restored {
        Some((sid, guard, session)) => (sid, guard, session),
        None => {
            let sid = value(args, "--session").unwrap_or_else(|| new_id("session"));
            if st.sessions().map_err(|e| e.to_string())?.contains(&sid) {
                return Err("session already exists; use `resume` to continue it".into());
            }
            st.create_session(&sid).map_err(|e| e.to_string())?;
            let guard = Arc::new(st.acquire_session(&sid).map_err(|e| e.to_string())?);
            (sid.clone(), guard, servoloop_core::Session::new(&sid))
        }
    };
    let mut seq = 0;
    control
        .output
        .emit(&mut seq, &sid, "session_started", None)?;
    let intent = JournalRecord {
        version: SCHEMA_VERSION,
        sequence: 0,
        session_id: sid.clone(),
        intent_id: new_id("intent"),
        kind: "intent".into(),
        arguments: redact_value(json!({"prompt": prompt}))?,
        outcome: None,
    };
    let harness = harness();
    let mut tools = ToolRegistry::new();
    harness
        .register_tools(&mut tools)
        .map_err(|e| e.to_string())?;
    let command_tool = tools
        .get("robot_command")
        .ok_or("robot command tool missing")?;
    let mut wrapped = ToolRegistry::new();
    wrapped
        .register_arc(Arc::new(JournalTool {
            inner: command_tool,
            guard: guard.clone(),
            intent,
        }))
        .map_err(|e| e.to_string())?;
    if let Some(observe) = tools.get("robot_observe") {
        wrapped.register_arc(observe).map_err(|e| e.to_string())?;
    }
    let mut loop_config = LoopConfig::default();
    if let Some(turns) = value(args, "--max-turns") {
        loop_config.max_turns = turns
            .parse()
            .map_err(|_| "--max-turns must be a positive integer")?;
        if loop_config.max_turns == 0 {
            return Err("--max-turns must be greater than zero".into());
        }
    }
    if let Some(seconds) = value(args, "--model-timeout") {
        loop_config.model_deadline = Some(std::time::Duration::from_secs(
            seconds
                .parse()
                .map_err(|_| "--model-timeout must be seconds")?,
        ));
    }
    if let Some(seconds) = value(args, "--tool-timeout") {
        loop_config.tool_deadline = Some(std::time::Duration::from_secs(
            seconds
                .parse()
                .map_err(|_| "--tool-timeout must be seconds")?,
        ));
    }
    let agent = AgentLoop::new(
        model,
        wrapped,
        "Operate the robot conservatively; verify every action.",
    )
    .with_config(loop_config);
    if session
        .messages
        .iter()
        .any(|m| matches!(m, servoloop_core::Message::ToolUnknown { .. }))
    {
        return Err("session has unresolved ToolUnknown results; refusing to resume".into());
    }
    if !session.messages.is_empty() && is_restored {
        session.messages.push(servoloop_core::Message::System {
            content: "Resume disclaimer: this is a fresh simulated environment. Prior robot observations are historical and are not current state; do not replay prior movement.".into(),
        });
    }
    let stop = control.stop;
    let signal_stop = stop.clone();
    let signal = control.listen_for_signal.then(|| {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_stop.stop();
                true
            } else {
                false
            }
        })
    });
    let event_seq = Arc::new(StdMutex::new(seq));
    let event_seq_sink = event_seq.clone();
    let event_error = Arc::new(StdMutex::new(None));
    let event_error_sink = event_error.clone();
    let run_result = agent
        .run(
            &mut session,
            prompt,
            &|event: AgentEvent| {
                if let Err(error) = control.output.emit(
                    &mut event_seq_sink.lock().expect("event sequence lock"),
                    &sid,
                    "agent_event",
                    serde_json::to_value(event).ok(),
                ) {
                    *event_error_sink.lock().expect("event error lock") = Some(error);
                }
            },
            &stop,
        )
        .await;
    let interrupted = stop.is_stopped();
    if let Some(signal) = signal {
        signal.abort();
    }
    let stop_error = if interrupted {
        // Keep emitting a terminal interrupted event even if the bounded
        // cleanup itself faults; a missing terminal event is unsafe for
        // machine consumers and leaves the outcome ambiguous.
        harness
            .emergency_stop()
            .await
            .err()
            .map(|error| error.to_string())
    } else {
        None
    };
    seq = *event_seq.lock().expect("event sequence lock");
    if let Some(error) = event_error.lock().expect("event error lock").clone() {
        return Err(format!("failed to redact agent event: {error}"));
    }
    if interrupted {
        // A stop racing the final model response is still an interrupted run:
        // never turn an action with uncertain timing into a success.
        control.output.emit(
            &mut seq,
            &sid,
            "session_finished",
            Some(
                json!({"outcome": "interrupted", "error": stop_error.unwrap_or_else(|| "run stopped; action outcome requires reconciliation".into())}),
            ),
        )?;
        return Err("run stopped; action outcome requires reconciliation".into());
    }
    match run_result {
        Ok(output) => {
            if let Err(error) = redacted_session(&session)
                .and_then(|session| guard.save_snapshot(&session).map_err(|e| e.to_string()))
            {
                control.output.emit(
                    &mut seq,
                    &sid,
                    "session_finished",
                    Some(json!({"outcome":"failed", "error":error})),
                )?;
                return Err("failed to save session snapshot".into());
            }
            control.output.emit(
                &mut seq,
                &sid,
                "verified",
                Some(json!({"simulated": simulated, "output": output})),
            )?;
            control.output.emit(
                &mut seq,
                &sid,
                "session_finished",
                Some(json!({"outcome":"success"})),
            )?;
            Ok(0)
        }
        Err(error) => {
            control.output.emit(
                &mut seq,
                &sid,
                "session_finished",
                Some(
                    json!({"outcome": if interrupted { "interrupted" } else { "failed" }, "error": error.to_string()}),
                ),
            )?;
            Err(error.to_string())
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use crate::simulation::SimulatedDriver;
    use async_trait::async_trait;
    use serde_json::Value;
    use servoloop_core::EventSink;
    use servoloop_core::{
        ModelRequest, ModelResponse, Result as CoreResult, Tool, ToolCall, ToolOutput,
    };
    use servoloop_robot::{JointLimitPolicy, RobotHarness, RobotState};
    use servoloop_store::Store;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    struct Events(StdMutex<Vec<AgentEvent>>);
    impl EventSink for Events {
        fn emit(&self, event: AgentEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    struct OneToolModel;
    #[async_trait]
    impl Model for OneToolModel {
        async fn complete(&self, _request: ModelRequest) -> CoreResult<ModelResponse> {
            Ok(ModelResponse {
                tool_calls: vec![ToolCall {
                    id: "move-1".into(),
                    name: "robot_command".into(),
                    arguments: json!({"command":"stop"}),
                }],
                ..Default::default()
            })
        }
    }

    struct ReadyThenHang {
        ready: Arc<Notify>,
        released: Arc<Notify>,
        started: AtomicBool,
    }
    #[async_trait]
    impl Tool for ReadyThenHang {
        fn definition(&self) -> servoloop_core::ToolDefinition {
            servoloop_core::ToolDefinition {
                name: "robot_command".into(),
                description: "test".into(),
                parameters: json!({"type":"object"}),
            }
        }
        async fn execute(&self, _args: Value) -> CoreResult<ToolOutput> {
            self.started.store(true, Ordering::SeqCst);
            self.ready.notify_one();
            self.released.notified().await;
            Ok(ToolOutput::text("unexpected completion"))
        }
    }

    #[tokio::test]
    async fn cancellation_leaves_pending_journal_and_awaits_stop() {
        let root = std::env::temp_dir().join(new_id("cli-cancel"));
        let store = Store::open(&root).unwrap();
        let sid = "cancel-session";
        store.create_session(sid).unwrap();
        let guard = Arc::new(store.acquire_session(sid).unwrap());
        let ready = Arc::new(Notify::new());
        let tool = Arc::new(ReadyThenHang {
            ready: ready.clone(),
            released: Arc::new(Notify::new()),
            started: AtomicBool::new(false),
        });
        let intent = JournalRecord {
            version: SCHEMA_VERSION,
            sequence: 0,
            session_id: sid.into(),
            intent_id: new_id("intent"),
            kind: "intent".into(),
            arguments: json!({}),
            outcome: None,
        };
        let mut tools = ToolRegistry::new();
        tools
            .register_arc(Arc::new(JournalTool {
                inner: tool.clone(),
                guard,
                intent,
            }))
            .unwrap();
        let agent = AgentLoop::new(Arc::new(OneToolModel), tools, "test");
        let stop = StopToken::new();
        let events = Arc::new(Events(StdMutex::new(Vec::new())));
        let stop_for_run = stop.clone();
        let events_for_run = events.clone();
        let task = tokio::spawn(async move {
            let mut session = servoloop_core::Session::new(sid);
            let result = agent
                .run(&mut session, "move", events_for_run.as_ref(), &stop_for_run)
                .await;
            (result, session)
        });
        ready.notified().await;
        stop.stop();
        let (result, session) = task.await.unwrap();
        assert!(matches!(result, Err(servoloop_core::Error::Stopped)));
        assert!(!events
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::RunCompleted { .. })));
        assert_eq!(store.unresolved(sid).unwrap().len(), 1);
        assert!(session.has_unresolved_unknowns());

        let driver = Arc::new(SimulatedDriver(Mutex::new(RobotState::default())));
        let harness = RobotHarness::new(driver, Arc::new(JointLimitPolicy::default()));
        harness.emergency_stop().await.unwrap();
        assert_eq!(
            harness.status().await,
            servoloop_robot::RobotStatus::StopAcknowledged
        );
        let _ = fs::remove_dir_all(root);
    }
}
