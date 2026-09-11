//! `servoloop`: a deliberately small machine-oriented operator CLI.
use async_trait::async_trait;
use clap::{error::ErrorKind, Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use servoloop_core::{
    AgentLoop, Event as AgentEvent, LoopConfig, Model, ModelRequest, ModelResponse,
    Result as CoreResult, StopToken, Tool, ToolCall, ToolOutput, ToolRegistry,
};
use servoloop_providers::{Discovery, DiscoveryOptions, OpenAiCompatProvider, ProviderSpec};
use servoloop_robot::{
    CommandReceipt, JointLimit, JointLimitPolicy, RobotCommand, RobotDriver, RobotHarness,
    RobotState,
};
use servoloop_store::{new_id, JournalRecord, Store, SCHEMA_VERSION};
use std::io::{self, IsTerminal, Read};
use std::sync::{Arc, Mutex as StdMutex};
use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: Option<u32>,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    store: Option<PathBuf>,
    driver: Option<String>,
}

/// Typed front-end for the operator CLI. The execution layer still receives
/// the original argv so this remains compatible with existing integrations.
#[derive(Debug, Parser)]
#[command(name = "servoloop", version, about = "ServoLoop robot operator CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Debug, Subcommand)]
enum Command {
    Run(RunArgs),
    Models(ModelsArgs),
    Providers,
    Config(ConfigArgs),
    Sessions(SessionsArgs),
}
#[derive(Debug, Args)]
struct CommonArgs {
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long, value_name = "FORMAT", value_parser = ["ndjson", "json"])]
    output: Option<String>,
}
#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long)]
    demo: bool,
    #[arg(long)]
    driver: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    prompt: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long, default_value_t = 50)]
    max_turns: usize,
    #[arg(long, value_name = "SECONDS")]
    model_timeout: Option<u64>,
    #[arg(long, value_name = "SECONDS")]
    tool_timeout: Option<u64>,
}
#[derive(Debug, Args)]
struct ModelsArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long)]
    offline: bool,
}
#[derive(Debug, Args)]
struct ConfigArgs {
    #[arg(default_value = "show")]
    action: String,
    #[arg(long)]
    config: Option<PathBuf>,
}
#[derive(Debug, Args)]
struct SessionsArgs {
    action: Option<String>,
    id: Option<String>,
    #[arg(long)]
    store: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Serialize)]
struct Event<'a> {
    version: u32,
    sequence: u64,
    session_id: &'a str,
    wall_time: u64,
    event: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}
fn emit(seq: &mut u64, sid: &str, event: &str, data: Option<Value>) {
    *seq += 1;
    let line = serde_json::to_string(&Event {
        version: 1,
        sequence: *seq,
        session_id: sid,
        wall_time: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        event,
        data,
    })
    .unwrap();
    println!("{}", redact(&line));
}
fn redact(input: &str) -> String {
    let mut out = input.to_string();
    for name in [
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "NVIDIA_API_KEY",
        "SERVOLOOP_API_KEY",
    ] {
        if let Ok(secret) = env::var(name) {
            if !secret.is_empty() {
                out = out.replace(&secret, "[REDACTED]");
            }
        }
    }
    out
}
fn redact_value(value: Value) -> Value {
    serde_json::from_str(&redact(&value.to_string())).unwrap_or(value)
}
fn safe_config(cfg: &Config) -> Value {
    let mut value = serde_json::to_value(cfg).unwrap_or(Value::Null);
    if let Some(url) = value
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        // Credentials in an URL are never useful in `config` output.
        if let Some(at) = url.find('@') {
            let scheme = url.find("://").map(|i| i + 3).unwrap_or(0);
            if at >= scheme {
                value["base_url"] =
                    Value::String(format!("{}[REDACTED]{}", &url[..scheme], &url[at..]));
            }
        }
    }
    value
}
fn init_config(args: &[String]) -> Result<(), String> {
    let path = value(args, "--config")
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);
    if path.exists() {
        return Err(format!("config already exists: {}", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("config: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, b"{\n  \"version\": 1\n}\n").map_err(|e| format!("config: {e}"))?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("config: {e}"));
    }
    println!("{}", path.display());
    Ok(())
}
fn usage() {
    eprintln!("usage: servoloop <run|providers|models|config|sessions> [options]\n  run --demo [--store DIR] [--session ID]\n  run --prompt TEXT --provider ID --model ID [--store DIR] [--session ID]\n  models --provider ID [--offline --model ID]\n  sessions list|show ID|delete ID");
}
fn value(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}
fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}
fn machine_output(args: &[String]) -> bool {
    has(args, "--json") || value(args, "--output").as_deref() == Some("json")
}
fn config_action(args: &[String]) -> &str {
    args.iter()
        .skip(1)
        .find(|arg| matches!(arg.as_str(), "init" | "validate" | "show"))
        .map(String::as_str)
        .unwrap_or("show")
}
fn validate_args(args: &[String]) -> Result<(), String> {
    let command = args.first().map(String::as_str).unwrap_or("");
    let flags: &[&str] = match command {
        "run" => &[
            "--demo",
            "--store",
            "--session",
            "--driver",
            "--provider",
            "--model",
            "--base-url",
            "--config",
            "--prompt",
            "--max-turns",
            "--model-timeout",
            "--tool-timeout",
        ],
        "models" => &[
            "--provider",
            "--model",
            "--base-url",
            "--offline",
            "--config",
        ],
        "config" => &["--config"],
        "sessions" => &["--store", "--config"],
        "providers" => &[],
        _ => return Err(format!("unknown command `{command}`")),
    };
    let value_flags = [
        "--store",
        "--session",
        "--driver",
        "--provider",
        "--model",
        "--base-url",
        "--config",
        "--prompt",
        "--max-turns",
        "--model-timeout",
        "--tool-timeout",
    ];
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with('-') && !flags.contains(&a.as_str()) {
            return Err(format!("unknown option `{a}`"));
        }
        if value_flags.contains(&a.as_str()) {
            let stdin_prompt = a == "--prompt" && args.get(i + 1).map(String::as_str) == Some("-");
            if args.get(i + 1).is_none() || (args[i + 1].starts_with('-') && !stdin_prompt) {
                return Err(format!("{a} requires a value"));
            }
            i += 1;
        }
        i += 1;
    }
    Ok(())
}
fn store(args: &[String], config: Option<&Config>) -> Result<Store, String> {
    let p = value(args, "--store")
        .map(PathBuf::from)
        .or_else(|| env::var_os("SERVOLOOP_STORE").map(PathBuf::from))
        .or_else(|| config.and_then(|c| c.store.clone()))
        .unwrap_or_else(default_store_path);
    Store::open(p).map_err(|e| e.to_string())
}
fn default_store_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ServoLoop")
            .join("store")
    } else {
        env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("servoloop")
    }
}
fn default_config_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ServoLoop/config.json")
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("servoloop/config.json")
    }
}
fn load_config(args: &[String]) -> Result<Config, String> {
    let explicit = value(args, "--config").is_some() || env::var_os("SERVOLOOP_CONFIG").is_some();
    let path = value(args, "--config")
        .map(PathBuf::from)
        .or_else(|| env::var_os("SERVOLOOP_CONFIG").map(PathBuf::from))
        .unwrap_or_else(default_config_path);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if !explicit && e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("config: {e}")),
    };
    let c: Config = serde_json::from_str(&text).map_err(|e| format!("config must be JSON: {e}"))?;
    if c.version != Some(1) {
        return Err(format!(
            "unsupported config version {:?}; expected 1",
            c.version
        ));
    }
    Ok(c)
}

fn prompt_from_args(args: &[String]) -> Result<String, String> {
    const MAX_STDIN_PROMPT_BYTES: u64 = 1024 * 1024;
    let prompt = value(args, "--prompt").ok_or("--prompt is required for live runs")?;
    if prompt != "-" {
        return Ok(prompt);
    }
    if io::stdin().is_terminal() {
        return Err("--prompt - reads stdin; refusing interactive terminal input".into());
    }
    let mut text = String::new();
    io::stdin()
        .take(MAX_STDIN_PROMPT_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("stdin: {e}"))?;
    if text.len() as u64 > MAX_STDIN_PROMPT_BYTES {
        return Err(format!(
            "stdin prompt exceeds {MAX_STDIN_PROMPT_BYTES} bytes"
        ));
    }
    if text.trim().is_empty() {
        return Err("stdin prompt is empty".into());
    }
    Ok(text)
}
fn provider(name: &str, base: Option<String>) -> Result<ProviderSpec, String> {
    let p = match name {
        "openai" => ProviderSpec::openai(env::var("OPENAI_API_KEY").unwrap_or_default()),
        "openrouter" => {
            ProviderSpec::openrouter(env::var("OPENROUTER_API_KEY").unwrap_or_default())
        }
        "nvidia" => ProviderSpec::nvidia(env::var("NVIDIA_API_KEY").unwrap_or_default()),
        "ollama" => ProviderSpec::ollama(),
        _ => return Err(format!("unknown provider `{name}`")),
    };
    Ok(base.map_or(p.clone(), |u| p.with_base_url(u)))
}

fn setting(
    args: &[String],
    flag: &str,
    environment: &str,
    config: Option<String>,
) -> Option<String> {
    value(args, flag)
        .or_else(|| env::var(environment).ok())
        .or(config)
}
async fn run(args: &[String], cfg: &Config) -> Result<i32, String> {
    if value(args, "--driver")
        .or_else(|| cfg.driver.clone())
        .as_deref()
        .unwrap_or("simulated")
        != "simulated"
    {
        return Err("only the simulated driver is implemented; Isaac is not available".into());
    }
    if !has(args, "--demo") {
        let name = setting(
            args,
            "--provider",
            "SERVOLOOP_PROVIDER",
            cfg.provider.clone(),
        )
        .ok_or("--provider is required")?;
        let model = setting(args, "--model", "SERVOLOOP_MODEL", cfg.model.clone())
            .ok_or("--model is required")?;
        let spec = provider(
            &name,
            setting(
                args,
                "--base-url",
                "SERVOLOOP_BASE_URL",
                cfg.base_url.clone(),
            ),
        )?;
        spec.validate().map_err(|e| format!("provider: {e}"))?;
        let prompt = prompt_from_args(args)?;
        let model: Arc<dyn Model> =
            Arc::new(OpenAiCompatProvider::new(spec, model).map_err(|e| e.to_string())?);
        return run_loop(args, cfg, model, prompt, false).await;
    }
    let st = store(args, Some(cfg))?;
    let sid = value(args, "--session").unwrap_or_else(|| new_id("session"));
    st.create_session(&sid).map_err(|e| e.to_string())?;
    if !st.unresolved(&sid).map_err(|e| e.to_string())?.is_empty() {
        return Err("session has an unresolved intent; refusing to resume automatically".into());
    }
    let mut seq = 0;
    emit(&mut seq, &sid, "session_started", None);
    let intent = JournalRecord {
        version: SCHEMA_VERSION,
        sequence: 0,
        session_id: sid.clone(),
        intent_id: new_id("intent"),
        kind: "intent".into(),
        arguments: json!({"command":"move_joint","joint":"shoulder","position":0.2,"driver":"simulated"}),
        outcome: None,
    };
    let driver = Arc::new(SimulatedDriver(Mutex::new(RobotState {
        joints: BTreeMap::from([(String::from("shoulder"), 0.0)]),
        battery_percent: Some(100.0),
        ..Default::default()
    })));
    let harness = RobotHarness::new(
        driver,
        Arc::new(JointLimitPolicy {
            limits: BTreeMap::from([(
                String::from("shoulder"),
                JointLimit {
                    min: -1.5,
                    max: 1.5,
                    max_step: 0.25,
                },
            )]),
            minimum_battery_percent: Some(10.0),
        }),
    );
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
            store: st.clone(),
            intent: intent.clone(),
        }))
        .map_err(|e| e.to_string())?;
    if let Some(observe) = tools.get("robot_observe") {
        wrapped.register_arc(observe).map_err(|e| e.to_string())?;
    }
    let agent = AgentLoop::new(
        Arc::new(DemoModel(Mutex::new(0))),
        wrapped,
        "Observe the simulated robot, then make one small verified move.",
    );
    let mut session = servoloop_core::Session::new(&sid);
    let stop = StopToken::new();
    let event_seq = Arc::new(StdMutex::new(seq));
    let event_seq_sink = event_seq.clone();
    let result = agent
        .run(
            &mut session,
            "Move the shoulder to 0.2 radians.",
            &|event: AgentEvent| {
                emit(
                    &mut event_seq_sink.lock().expect("event sequence lock"),
                    &sid,
                    "agent_event",
                    serde_json::to_value(event).ok(),
                );
            },
            &stop,
        )
        .await;
    seq = *event_seq.lock().expect("event sequence lock");
    let output = result.map_err(|e| e.to_string())?;
    emit(
        &mut seq,
        &sid,
        "verified",
        Some(json!({"simulated":true,"output":output})),
    );
    emit(
        &mut seq,
        &sid,
        "session_finished",
        Some(json!({"outcome":"success"})),
    );
    Ok(0)
}

async fn run_loop(
    args: &[String],
    cfg: &Config,
    model: Arc<dyn Model>,
    prompt: String,
    simulated: bool,
) -> Result<i32, String> {
    if !machine_output(args) {
        eprintln!(
            "starting {} run",
            if simulated { "simulated" } else { "provider" }
        );
    }
    let st = store(args, Some(cfg))?;
    let sid = value(args, "--session").unwrap_or_else(|| new_id("session"));
    st.create_session(&sid).map_err(|e| e.to_string())?;
    if !st.unresolved(&sid).map_err(|e| e.to_string())?.is_empty() {
        return Err("session has an unresolved intent; refusing to resume automatically".into());
    }
    let mut seq = 0;
    emit(&mut seq, &sid, "session_started", None);
    let intent = JournalRecord {
        version: SCHEMA_VERSION,
        sequence: 0,
        session_id: sid.clone(),
        intent_id: new_id("intent"),
        kind: "intent".into(),
        arguments: redact_value(json!({"prompt": prompt})),
        outcome: None,
    };
    let driver = Arc::new(SimulatedDriver(Mutex::new(RobotState {
        joints: BTreeMap::from([(String::from("shoulder"), 0.0)]),
        battery_percent: Some(100.0),
        ..Default::default()
    })));
    let harness = RobotHarness::new(
        driver,
        Arc::new(JointLimitPolicy {
            limits: BTreeMap::from([(
                String::from("shoulder"),
                JointLimit {
                    min: -1.5,
                    max: 1.5,
                    max_step: 0.25,
                },
            )]),
            minimum_battery_percent: Some(10.0),
        }),
    );
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
            store: st.clone(),
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
    let mut session = servoloop_core::Session::new(&sid);
    let stop = StopToken::new();
    let signal_stop = stop.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_stop.stop();
            true
        } else {
            false
        }
    });
    let event_seq = Arc::new(StdMutex::new(seq));
    let event_seq_sink = event_seq.clone();
    let run_result = agent
        .run(
            &mut session,
            prompt,
            &|event: AgentEvent| {
                emit(
                    &mut event_seq_sink.lock().expect("event sequence lock"),
                    &sid,
                    "agent_event",
                    serde_json::to_value(event).ok(),
                );
            },
            &stop,
        )
        .await;
    let interrupted = stop.is_stopped();
    signal.abort();
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
    if interrupted {
        // A stop racing the final model response is still an interrupted run:
        // never turn an action with uncertain timing into a success.
        emit(
            &mut seq,
            &sid,
            "session_finished",
            Some(
                json!({"outcome": "interrupted", "error": stop_error.unwrap_or_else(|| "run stopped; action outcome requires reconciliation".into())}),
            ),
        );
        return Err("run stopped; action outcome requires reconciliation".into());
    }
    match run_result {
        Ok(output) => {
            emit(
                &mut seq,
                &sid,
                "verified",
                Some(json!({"simulated": simulated, "output": output})),
            );
            emit(
                &mut seq,
                &sid,
                "session_finished",
                Some(json!({"outcome":"success"})),
            );
            Ok(0)
        }
        Err(error) => {
            emit(
                &mut seq,
                &sid,
                "session_finished",
                Some(
                    json!({"outcome": if interrupted { "interrupted" } else { "failed" }, "error": error.to_string()}),
                ),
            );
            Err(error.to_string())
        }
    }
}

struct SimulatedDriver(Mutex<RobotState>);
#[async_trait]
impl RobotDriver for SimulatedDriver {
    async fn observe(&self) -> CoreResult<RobotState> {
        Ok(self.0.lock().await.clone())
    }
    async fn execute(&self, command: RobotCommand) -> CoreResult<CommandReceipt> {
        if let RobotCommand::MoveJoint { joint, position } = command {
            self.0.lock().await.joints.insert(joint, position);
        }
        Ok(CommandReceipt {
            accepted: true,
            message: "simulator accepted command".into(),
            metadata: Value::Null,
        })
    }
    async fn stop(&self) -> CoreResult<()> {
        Ok(())
    }
}

struct DemoModel(Mutex<u8>);
#[async_trait]
impl Model for DemoModel {
    async fn complete(&self, _request: ModelRequest) -> CoreResult<ModelResponse> {
        let mut n = self.0.lock().await;
        let response = match *n {
            0 => ModelResponse {
                content: "Inspecting first.".into(),
                tool_calls: vec![ToolCall {
                    id: "observe-1".into(),
                    name: "robot_observe".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
            1 => ModelResponse {
                content: "Making the requested small move.".into(),
                tool_calls: vec![ToolCall {
                    id: "move-1".into(),
                    name: "robot_command".into(),
                    arguments: json!({"command":"move_joint","joint":"shoulder","position":0.2}),
                }],
                ..Default::default()
            },
            _ => ModelResponse::text("Move completed and verified."),
        };
        *n += 1;
        Ok(response)
    }
}

struct JournalTool {
    inner: Arc<dyn Tool>,
    store: Store,
    intent: JournalRecord,
}
#[async_trait]
impl Tool for JournalTool {
    fn definition(&self) -> servoloop_core::ToolDefinition {
        let mut d = self.inner.definition();
        d.name = "robot_command".into();
        d
    }
    async fn execute(&self, args: Value) -> CoreResult<ToolOutput> {
        let mut intent = self.intent.clone();
        intent.arguments = redact_value(args.clone());
        self.store.append(intent.clone()).map_err(|e| {
            servoloop_core::Error::Tool(format!("journal failed; motion not dispatched: {e}"))
        })?;
        match self.inner.execute(args).await {
            Ok(out) => {
                let mut done = intent;
                done.kind = "result".into();
                done.outcome = Some("verified".into());
                self.store
                    .append(done)
                    .map_err(|e| servoloop_core::Error::Tool(e.to_string()))?;
                Ok(out)
            }
            Err(e) => {
                let mut failed = intent;
                // A driver error does not prove that no physical effect occurred.
                // Keep the intent unresolved so resumption is refused until an
                // operator reconciles the robot state.
                failed.kind = "result".into();
                failed.outcome = None;
                let _ = self.store.append(failed);
                Err(e)
            }
        }
    }
}
#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    // A lone `-` is a meaningful prompt value (stdin), but clap otherwise
    // treats it as the beginning of another option.
    let mut parse_args = args.clone();
    for i in 0..parse_args.len().saturating_sub(1) {
        if parse_args[i] == "--prompt" && parse_args[i + 1] == "-" {
            parse_args[i] = "--prompt=-".into();
            parse_args.remove(i + 1);
            break;
        }
    }
    let parse = std::iter::once(String::from("servoloop")).chain(parse_args);
    if let Err(error) = Cli::try_parse_from(parse) {
        let code = match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
            _ => 2,
        };
        let rendered = error
            .to_string()
            .replace("unexpected argument", "unknown option");
        eprint!("{rendered}");
        return ExitCode::from(code);
    }
    if args.is_empty() {
        usage();
        return ExitCode::from(2);
    }
    let command = &args[0];
    if let Err(e) = validate_args(&args) {
        eprintln!("error: {e}");
        return ExitCode::from(2);
    }
    if command == "providers" {
        println!(
            "{}",
            json!({"version":1,"providers":["openai","openrouter","nvidia","ollama"]})
        );
        return ExitCode::SUCCESS;
    }
    if command == "config" && config_action(&args) == "init" {
        return match init_config(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(1)
            }
        };
    }
    let cfg = match load_config(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let result: Result<i32, String> = match command.as_str() {
        "run" => run(&args, &cfg).await,
        "models" => models(&args, &cfg).await,
        "config" => match config_action(&args) {
            "init" => init_config(&args).map(|_| 0),
            "validate" => {
                println!("valid");
                Ok(0)
            }
            "show" => {
                println!("{}", redact(&safe_config(&cfg).to_string()));
                Ok(0)
            }
            action => Err(format!("unknown config action `{action}`")),
        },
        "sessions" => sessions(&args, &cfg).map(|_| 0),
        _ => {
            usage();
            Err("unknown command".into())
        }
    };
    match result {
        Ok(c) => ExitCode::from(c as u8),
        Err(e) => {
            eprintln!("error: {}", redact(&e));
            ExitCode::from(2)
        }
    }
}
async fn models(args: &[String], cfg: &Config) -> Result<i32, String> {
    let name = setting(
        args,
        "--provider",
        "SERVOLOOP_PROVIDER",
        cfg.provider.clone(),
    )
    .ok_or("--provider is required")?;
    let model = setting(args, "--model", "SERVOLOOP_MODEL", cfg.model.clone());
    let spec = provider(
        &name,
        setting(
            args,
            "--base-url",
            "SERVOLOOP_BASE_URL",
            cfg.base_url.clone(),
        ),
    )?;
    let opts = DiscoveryOptions {
        explicit_model_ids: model.into_iter().collect(),
        ..Default::default()
    };
    if has(args, "--offline") {
        if opts.explicit_model_ids.is_empty() {
            return Err("--offline requires --model".into());
        }
        println!(
            "{}",
            serde_json::to_string(&opts.explicit_model_ids).unwrap()
        );
        return Ok(0);
    };
    let found = Discovery::new()
        .discover(&spec, &opts)
        .await
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string(&found).unwrap());
    Ok(0)
}
fn sessions(args: &[String], cfg: &Config) -> Result<(), String> {
    let st = store(args, Some(cfg))?;
    match args.get(1).map(String::as_str) {
        Some("list") | None => println!("{}", json!(st.sessions().map_err(|e| e.to_string())?)),
        Some("show") => println!(
            "{}",
            redact(
                &serde_json::to_string(
                    &st.records(args.get(2).ok_or("session ID required")?)
                        .map_err(|e| e.to_string())?
                )
                .unwrap()
            )
        ),
        Some("delete") => {
            st.delete(args.get(2).ok_or("session ID required")?)
                .map_err(|e| e.to_string())?;
            println!("deleted")
        }
        _ => return Err("sessions requires list, show, or delete".into()),
    }
    Ok(())
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use servoloop_core::EventSink;
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
                store: store.clone(),
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
