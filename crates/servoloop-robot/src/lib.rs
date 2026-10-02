//! Hardware-neutral, simulation-only robot control.
//!
//! This is a command *coordination* layer, not a hardware safety controller.
//! A real deployment must have an independent safety controller; dropping a
//! Tokio task cannot guarantee that hardware has stopped.

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::{oneshot, watch, Mutex};
use unravel_agent_runtime::{Error, Result, Tool, ToolDefinition, ToolOutput, ToolRegistry};

fn safety_error(message: String) -> Error {
    Error::Policy(format!("safety violation: {message}"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RobotState {
    pub joints: BTreeMap<String, f64>,
    pub battery_percent: Option<f64>,
    pub emergency_stop: bool,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum RobotCommand {
    MoveJoint { joint: String, position: f64 },
    MoveJoints { positions: BTreeMap<String, f64> },
    SetOutput { channel: String, value: f64 },
    Stop,
}
impl<'de> Deserialize<'de> for RobotCommand {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("command must be an object"))?;
        let command = object
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("command is required"))?;
        let allowed = match command {
            "move_joint" => &["command", "joint", "position"][..],
            "move_joints" => &["command", "positions"][..],
            "set_output" => &["command", "channel", "value"][..],
            "stop" => &["command"][..],
            _ => return Err(serde::de::Error::custom("unknown command")),
        };
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(serde::de::Error::custom("unknown command field"));
        }
        match command {
            "move_joint" => Ok(Self::MoveJoint {
                joint: serde_json::from_value(
                    object
                        .get("joint")
                        .cloned()
                        .ok_or_else(|| serde::de::Error::custom("joint is required"))?,
                )
                .map_err(serde::de::Error::custom)?,
                position: serde_json::from_value(
                    object
                        .get("position")
                        .cloned()
                        .ok_or_else(|| serde::de::Error::custom("position is required"))?,
                )
                .map_err(serde::de::Error::custom)?,
            }),
            "move_joints" => Ok(Self::MoveJoints {
                positions: serde_json::from_value(
                    object
                        .get("positions")
                        .cloned()
                        .ok_or_else(|| serde::de::Error::custom("positions is required"))?,
                )
                .map_err(serde::de::Error::custom)?,
            }),
            "set_output" => Ok(Self::SetOutput {
                channel: serde_json::from_value(
                    object
                        .get("channel")
                        .cloned()
                        .ok_or_else(|| serde::de::Error::custom("channel is required"))?,
                )
                .map_err(serde::de::Error::custom)?,
                value: serde_json::from_value(
                    object
                        .get("value")
                        .cloned()
                        .ok_or_else(|| serde::de::Error::custom("value is required"))?,
                )
                .map_err(serde::de::Error::custom)?,
            }),
            _ => Ok(Self::Stop),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandReceipt {
    pub accepted: bool,
    pub message: String,
    #[serde(default)]
    pub metadata: Value,
}

#[async_trait]
pub trait RobotDriver: Send + Sync {
    async fn observe(&self) -> Result<RobotState>;
    async fn execute(&self, command: RobotCommand) -> Result<CommandReceipt>;
    async fn stop(&self) -> Result<()>;
}

pub trait SafetyPolicy: Send + Sync {
    fn validate(&self, command: &RobotCommand, state: &RobotState) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct JointLimit {
    pub min: f64,
    pub max: f64,
    pub max_step: f64,
}

#[derive(Debug, Clone, Default)]
pub struct JointLimitPolicy {
    pub limits: BTreeMap<String, JointLimit>,
    pub minimum_battery_percent: Option<f64>,
}

impl SafetyPolicy for JointLimitPolicy {
    fn validate(&self, command: &RobotCommand, state: &RobotState) -> Result<()> {
        for (joint, limit) in &self.limits {
            if !limit.min.is_finite()
                || !limit.max.is_finite()
                || !limit.max_step.is_finite()
                || limit.min > limit.max
                || limit.max_step < 0.0
            {
                return Err(safety_error(format!("invalid limits for joint `{joint}`")));
            }
        }
        if let Some(minimum) = self.minimum_battery_percent {
            if !minimum.is_finite() || !(0.0..=100.0).contains(&minimum) {
                return Err(safety_error("minimum battery threshold is invalid".into()));
            }
            let actual = state
                .battery_percent
                .ok_or_else(|| safety_error("battery reading is missing".into()))?;
            if !actual.is_finite() || !(0.0..=100.0).contains(&actual) {
                return Err(safety_error("battery reading is invalid".into()));
            }
            if actual < minimum && !matches!(command, RobotCommand::Stop) {
                return Err(safety_error(format!(
                    "battery is {actual:.1}%, below the {minimum:.1}% minimum"
                )));
            }
        } else if let Some(actual) = state.battery_percent {
            if !actual.is_finite() || !(0.0..=100.0).contains(&actual) {
                return Err(safety_error("battery reading is invalid".into()));
            }
        }
        if state.joints.values().any(|v| !v.is_finite()) {
            return Err(safety_error("observed joint position is invalid".into()));
        }
        if state.emergency_stop && !matches!(command, RobotCommand::Stop) {
            return Err(safety_error("the robot reports an emergency stop".into()));
        }
        match command {
            RobotCommand::MoveJoint { joint, position } => {
                self.validate_joint(joint, *position, state)
            }
            RobotCommand::MoveJoints { positions } => {
                if positions.is_empty() {
                    return Err(safety_error(
                        "move_joints requires at least one joint".into(),
                    ));
                }
                for (joint, position) in positions {
                    self.validate_joint(joint, *position, state)?;
                }
                Ok(())
            }
            RobotCommand::SetOutput { .. } => Err(safety_error(
                "output channels are not authorized by the joint-only policy".into(),
            )),
            RobotCommand::Stop => Ok(()),
        }
    }
}
impl JointLimitPolicy {
    fn validate_joint(&self, joint: &str, target: f64, state: &RobotState) -> Result<()> {
        if !target.is_finite() {
            return Err(safety_error(format!(
                "joint `{joint}` target must be finite"
            )));
        }
        let limit = self
            .limits
            .get(joint)
            .ok_or_else(|| safety_error(format!("joint `{joint}` is not configured")))?;
        if !(limit.min..=limit.max).contains(&target) {
            return Err(safety_error(format!(
                "joint `{joint}` target {target} is outside [{}, {}]",
                limit.min, limit.max
            )));
        }
        let current = state
            .joints
            .get(joint)
            .ok_or_else(|| safety_error(format!("joint `{joint}` has no observed position")))?;
        if (target - current).abs() > limit.max_step {
            return Err(safety_error(format!(
                "joint `{joint}` move exceeds the maximum step of {}",
                limit.max_step
            )));
        }
        Ok(())
    }
}

#[derive(Default)]
struct Lifecycle {
    fault: Option<String>,
    stop_requested: bool,
    stop_acknowledged: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotStatus {
    Ready,
    StopAcknowledged,
    Fault(String),
}
struct Coordinator {
    gate: Mutex<()>,
    stop_gate: Mutex<()>,
    lifecycle: Mutex<Lifecycle>,
    stop_generation: watch::Sender<u64>,
}
struct CancelOnDrop(Option<oneshot::Sender<()>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[derive(Clone)]
pub struct RobotHarness {
    driver: Arc<dyn RobotDriver>,
    policy: Arc<dyn SafetyPolicy>,
    coordinator: Arc<Coordinator>,
    observe_timeout: Duration,
    execute_timeout: Duration,
    stop_timeout: Duration,
    queue_timeout: Duration,
    tolerance: f64,
}
impl RobotHarness {
    pub fn new(driver: Arc<dyn RobotDriver>, policy: Arc<dyn SafetyPolicy>) -> Self {
        Self {
            driver,
            policy,
            coordinator: Arc::new(Coordinator {
                gate: Mutex::new(()),
                stop_gate: Mutex::new(()),
                lifecycle: Mutex::new(Lifecycle::default()),
                stop_generation: watch::channel(0).0,
            }),
            observe_timeout: Duration::from_secs(5),
            execute_timeout: Duration::from_secs(5),
            stop_timeout: Duration::from_secs(2),
            queue_timeout: Duration::from_secs(5),
            tolerance: 1e-6,
        }
    }
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.observe_timeout = timeout;
        self.execute_timeout = timeout;
        self
    }
    pub async fn status(&self) -> RobotStatus {
        let l = self.coordinator.lifecycle.lock().await;
        if let Some(fault) = &l.fault {
            RobotStatus::Fault(fault.clone())
        } else if l.stop_acknowledged {
            RobotStatus::StopAcknowledged
        } else {
            RobotStatus::Ready
        }
    }
    pub fn with_observe_timeout(mut self, timeout: Duration) -> Self {
        self.observe_timeout = timeout;
        self
    }
    pub fn with_execute_timeout(mut self, timeout: Duration) -> Self {
        self.execute_timeout = timeout;
        self
    }
    pub fn with_stop_timeout(mut self, timeout: Duration) -> Self {
        self.stop_timeout = timeout;
        self
    }
    /// Bound admission to the serialized command lifecycle. Expiry rejects the
    /// waiting command without stopping or dispatching another caller's motion.
    pub fn with_queue_timeout(mut self, timeout: Duration) -> Self {
        self.queue_timeout = timeout;
        self
    }
    pub fn with_postcondition_tolerance(mut self, tolerance: f64) -> Self {
        self.tolerance = if tolerance.is_finite() && tolerance >= 0.0 {
            tolerance
        } else {
            f64::NAN
        };
        self
    }
    pub async fn emergency_stop(&self) -> Result<()> {
        {
            let mut l = self.coordinator.lifecycle.lock().await;
            l.stop_requested = true;
            l.stop_acknowledged = false;
            let generation = *self.coordinator.stop_generation.borrow() + 1;
            self.coordinator.stop_generation.send_replace(generation);
        }
        let this = self.clone();
        tokio::spawn(async move { this.perform_stop().await })
            .await
            .map_err(|e| Error::Tool(format!("robot stop supervisor failed: {e}")))?
    }
    async fn perform_stop(&self) -> Result<()> {
        let _stop_gate = match tokio::time::timeout(
            self.stop_timeout,
            self.coordinator.stop_gate.lock(),
        )
        .await
        {
            Ok(guard) => guard,
            Err(_) => {
                self.latch("stop queue timed out".into()).await;
                return Err(Error::Tool(
                    "robot stop queue timed out; robot is faulted".into(),
                ));
            }
        };
        match tokio::time::timeout(self.stop_timeout, self.driver.stop()).await {
            Ok(Ok(())) => {
                self.coordinator.lifecycle.lock().await.stop_acknowledged = true;
                Ok(())
            }
            Ok(Err(e)) => {
                self.latch(format!("stop failed: {e}")).await;
                Err(e)
            }
            Err(_) => {
                self.latch("stop timed out".into()).await;
                Err(Error::Tool("robot stop timed out; robot is faulted".into()))
            }
        }
    }
    /// Clears an emergency stop only after a completed, acknowledged stop.
    /// Fault recovery is an explicit operator action and requires a fresh,
    /// bounded, policy-checked observation after the stop acknowledgement.
    pub async fn reset_emergency_stop(&self) -> Result<()> {
        let _gate = tokio::time::timeout(self.stop_timeout, self.coordinator.gate.lock())
            .await
            .map_err(|_| Error::Tool("robot reset queue timed out".into()))?;
        let _stop_gate = tokio::time::timeout(self.stop_timeout, self.coordinator.stop_gate.lock())
            .await
            .map_err(|_| Error::Tool("robot stop queue timed out".into()))?;
        let reset_generation = *self.coordinator.stop_generation.borrow();
        {
            let l = self.coordinator.lifecycle.lock().await;
            if !l.stop_acknowledged {
                return Err(safety_error(
                    "robot has not completed an acknowledged stop".into(),
                ));
            }
        }
        let state = self.observe_bounded().await?;
        self.policy.validate(&RobotCommand::Stop, &state)?;
        if state.emergency_stop {
            return Err(safety_error("driver still reports emergency stop".into()));
        }
        let mut l = self.coordinator.lifecycle.lock().await;
        if !l.stop_acknowledged || *self.coordinator.stop_generation.borrow() != reset_generation {
            return Err(safety_error("stop state changed during reset".into()));
        }
        l.stop_acknowledged = false;
        l.stop_requested = false;
        l.fault = None;
        Ok(())
    }
    pub fn register_tools(&self, registry: &mut ToolRegistry) -> Result<()> {
        registry.register(RobotObserveTool(self.clone()))?;
        registry.register(RobotCommandTool(self.clone()))?;
        Ok(())
    }
    async fn latch(&self, reason: String) {
        let mut l = self.coordinator.lifecycle.lock().await;
        l.fault = Some(reason);
        l.stop_requested = true;
        l.stop_acknowledged = false;
    }
    async fn observe_bounded(&self) -> Result<RobotState> {
        tokio::time::timeout(self.observe_timeout, self.driver.observe())
            .await
            .map_err(|_| Error::Tool("robot observation timed out".into()))?
    }
    async fn execute(&self, command: RobotCommand) -> Result<CommandReceipt> {
        if matches!(command, RobotCommand::Stop) {
            self.emergency_stop().await?;
            return Ok(CommandReceipt {
                accepted: true,
                message: "robot stopped".into(),
                metadata: Value::Null,
            });
        }
        // The owned supervisor continues after the caller drops this future and performs
        // cleanup on execute/post-observe uncertainty instead of abandoning the actuator.
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let this = self.clone();
        let supervisor =
            tokio::spawn(async move { this.lifecycle_command(command, cancel_rx).await });
        let _cancel = CancelOnDrop(Some(cancel_tx));
        supervisor
            .await
            .map_err(|e| Error::Tool(format!("robot supervisor failed: {e}")))?
    }
    async fn lifecycle_command(
        &self,
        command: RobotCommand,
        mut cancel: oneshot::Receiver<()>,
    ) -> Result<CommandReceipt> {
        let mut stop = self.coordinator.stop_generation.subscribe();
        let generation = *stop.borrow();
        let _gate = tokio::select! {
            biased;
            gate = tokio::time::timeout(self.queue_timeout, self.coordinator.gate.lock()) => {
                gate.map_err(|_| Error::Tool("robot command queue timed out; command not dispatched".into()))?
            },
            _ = &mut cancel => return Err(Error::Stopped),
            result = stop.changed() => {
                if result.is_ok() { return Err(Error::Stopped); }
                return Err(safety_error("stop coordinator closed".into()));
            }
        };
        {
            let l = self.coordinator.lifecycle.lock().await;
            if let Some(f) = &l.fault {
                return Err(safety_error(format!("robot is faulted: {f}")));
            }
            if l.stop_requested || l.stop_acknowledged {
                return Err(safety_error("ServoLoop emergency stop is engaged".into()));
            }
        }
        let state = match tokio::select! {
            biased;
            result = self.observe_bounded() => result,
            _ = &mut cancel => { self.cleanup_fault("command cancelled before observation".into()).await; return Err(Error::Stopped); },
            result = stop.changed() => { if result.is_ok() { return Err(Error::Stopped); } return Err(safety_error("stop coordinator closed".into())); }
        } {
            Ok(state) => state,
            Err(error) => {
                self.cleanup_fault(format!("pre-action observation failed: {error}"))
                    .await;
                return Err(error);
            }
        };
        if !self.tolerance.is_finite() || self.tolerance < 0.0 {
            return Err(safety_error("postcondition tolerance is invalid".into()));
        }
        self.policy.validate(&command, &state)?;
        if *self.coordinator.stop_generation.borrow() != generation {
            return Err(Error::Stopped);
        }
        {
            let l = self.coordinator.lifecycle.lock().await;
            if l.stop_requested || l.stop_acknowledged {
                return Err(Error::Stopped);
            }
        }
        if matches!(
            cancel.try_recv(),
            Ok(_) | Err(oneshot::error::TryRecvError::Closed)
        ) {
            return Err(Error::Stopped);
        }
        let receipt = match tokio::select! {
            biased;
            result = tokio::time::timeout(self.execute_timeout, self.driver.execute(command.clone())) => result,
            _ = &mut cancel => { self.cleanup_fault("command cancelled during execute".into()).await; return Err(Error::Stopped); },
            result = stop.changed() => { if result.is_ok() { self.cleanup_fault("stop requested during execute".into()).await; } return Err(Error::Stopped); }
        } {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                self.cleanup_fault(format!("execute failed: {e}")).await;
                return Err(e);
            }
            Err(_) => {
                self.cleanup_fault("execute timed out".into()).await;
                return Err(Error::Tool(
                    "robot command timed out; stop was requested".into(),
                ));
            }
        };
        if !receipt.accepted {
            return Err(Error::Tool(format!(
                "robot rejected command: {} ({})",
                receipt.message, receipt.metadata
            )));
        }
        let after = match tokio::select! {
            biased;
            result = self.observe_bounded() => result,
            _ = &mut cancel => { self.cleanup_fault("command cancelled during verification".into()).await; return Err(Error::Stopped); },
            result = stop.changed() => { if result.is_ok() { self.cleanup_fault("stop requested during verification".into()).await; } return Err(Error::Stopped); }
        } {
            Ok(s) => s,
            Err(e) => {
                self.cleanup_fault(format!("post-action observation failed: {e}"))
                    .await;
                return Err(e);
            }
        };
        if let Err(e) = verify_target(&command, &after, self.tolerance) {
            self.cleanup_fault(format!("post-action verification failed: {e}"))
                .await;
            return Err(e);
        }
        Ok(CommandReceipt {
            accepted: true,
            message: receipt.message,
            metadata: json!({"receipt": receipt.metadata, "post_action_state": after}),
        })
    }
    async fn cleanup_fault(&self, reason: String) {
        let _stop_gate = match tokio::time::timeout(
            self.stop_timeout,
            self.coordinator.stop_gate.lock(),
        )
        .await
        {
            Ok(guard) => guard,
            Err(_) => {
                self.latch(format!("{reason}; cleanup stop queue timed out"))
                    .await;
                return;
            }
        };
        self.latch(reason.clone()).await;
        match tokio::time::timeout(self.stop_timeout, self.driver.stop()).await {
            Ok(Ok(())) => {
                self.coordinator.lifecycle.lock().await.stop_acknowledged = true;
            }
            Ok(Err(error)) => {
                self.latch(format!("{reason}; cleanup stop failed: {error}"))
                    .await;
            }
            Err(_) => {
                self.latch(format!("{reason}; cleanup stop timed out"))
                    .await;
            }
        }
    }
}
fn verify_target(command: &RobotCommand, state: &RobotState, tolerance: f64) -> Result<()> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err(safety_error("postcondition tolerance is invalid".into()));
    }
    let targets: Box<dyn Iterator<Item = (&String, &f64)> + '_> = match command {
        RobotCommand::MoveJoint { joint, position } => Box::new(std::iter::once((joint, position))),
        RobotCommand::MoveJoints { positions } => Box::new(positions.iter()),
        _ => return Ok(()),
    };
    for (joint, target) in targets {
        let actual = state
            .joints
            .get(joint)
            .ok_or_else(|| safety_error(format!("post-action joint `{joint}` is missing")))?;
        if !actual.is_finite() || (actual - target).abs() > tolerance {
            return Err(safety_error(format!(
                "post-action joint `{joint}` does not match target"
            )));
        }
    }
    Ok(())
}

struct RobotObserveTool(RobotHarness);
#[async_trait]
impl Tool for RobotObserveTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "robot_observe".into(),
            description: "Read the robot state before deciding on a command.".into(),
            parameters: json!({"type":"object","additionalProperties":false}),
        }
    }
    async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
        let state = self.0.observe_bounded().await?;
        self.0.policy.validate(&RobotCommand::Stop, &state)?;
        Ok(ToolOutput::with_metadata(
            serde_json::to_string(&state).map_err(|e| Error::Tool(e.to_string()))?,
            serde_json::to_value(state).map_err(|e| Error::Tool(e.to_string()))?,
        ))
    }
}
struct RobotCommandTool(RobotHarness);
#[async_trait]
impl Tool for RobotCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "robot_command".into(),
            description: "Send one safety-checked command to the robot.".into(),
            parameters: json!({"oneOf":[
                {"type":"object","properties":{"command":{"const":"move_joint"},"joint":{"type":"string","minLength":1},"position":{"type":"number"}},"required":["command","joint","position"],"additionalProperties":false},
                {"type":"object","properties":{"command":{"const":"move_joints"},"positions":{"type":"object","minProperties":1,"additionalProperties":{"type":"number"}}},"required":["command","positions"],"additionalProperties":false},
                {"type":"object","properties":{"command":{"const":"set_output"},"channel":{"type":"string","minLength":1},"value":{"type":"number"}},"required":["command","channel","value"],"additionalProperties":false},
                {"type":"object","properties":{"command":{"const":"stop"}},"required":["command"],"additionalProperties":false}
            ]}),
        }
    }
    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let command: RobotCommand =
            serde_json::from_value(arguments).map_err(|e| Error::InvalidInput(e.to_string()))?;
        let receipt = self.0.execute(command).await?;
        Ok(ToolOutput::with_metadata(
            receipt.message.clone(),
            serde_json::to_value(receipt).map_err(|e| Error::Tool(e.to_string()))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };
    use tokio::sync::Notify;
    use unravel_agent_runtime::{
        AgentLoop, FinishReason, Model, ModelRequest, ModelResponse, NoopEventSink, Session,
        StopToken, ToolCall,
    };
    struct Fake(Mutex<RobotState>);
    #[async_trait]
    impl RobotDriver for Fake {
        async fn observe(&self) -> Result<RobotState> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn execute(&self, c: RobotCommand) -> Result<CommandReceipt> {
            if let RobotCommand::MoveJoint { joint, position } = c {
                self.0.lock().unwrap().joints.insert(joint, position);
            }
            Ok(CommandReceipt {
                accepted: true,
                message: "accepted".into(),
                metadata: Value::Null,
            })
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
    }
    fn harness() -> RobotHarness {
        RobotHarness::new(
            Arc::new(Fake(Mutex::new(RobotState {
                joints: BTreeMap::from([(String::from("shoulder"), 0.0)]),
                battery_percent: Some(80.0),
                ..Default::default()
            }))),
            Arc::new(JointLimitPolicy {
                limits: BTreeMap::from([(
                    String::from("shoulder"),
                    JointLimit {
                        min: -1.0,
                        max: 1.0,
                        max_step: 0.25,
                    },
                )]),
                minimum_battery_percent: Some(10.0),
            }),
        )
    }
    #[tokio::test]
    async fn accepts_move_and_returns_post_state() {
        let r = harness()
            .execute(RobotCommand::MoveJoint {
                joint: "shoulder".into(),
                position: 0.2,
            })
            .await
            .unwrap();
        assert!(r.metadata["post_action_state"]["joints"]["shoulder"] == json!(0.2));
    }
    #[tokio::test]
    async fn rejects_large_move() {
        assert!(matches!(
            harness()
                .execute(RobotCommand::MoveJoint {
                    joint: "shoulder".into(),
                    position: 0.5
                })
                .await,
            Err(Error::Policy(_))
        ));
    }
    #[tokio::test]
    async fn stop_blocks_motion_until_reset() {
        let h = harness();
        h.emergency_stop().await.unwrap();
        assert!(h
            .execute(RobotCommand::MoveJoint {
                joint: "shoulder".into(),
                position: 0.1
            })
            .await
            .is_err());
        h.reset_emergency_stop().await.unwrap();
        assert!(h
            .execute(RobotCommand::MoveJoint {
                joint: "shoulder".into(),
                position: 0.1
            })
            .await
            .is_ok());
    }
    #[test]
    fn rejects_output() {
        let p = JointLimitPolicy::default();
        assert!(p
            .validate(
                &RobotCommand::SetOutput {
                    channel: "x".into(),
                    value: 1.0
                },
                &RobotState::default()
            )
            .is_err());
    }
    #[test]
    fn rejects_empty_batch() {
        let p = JointLimitPolicy::default();
        assert!(p
            .validate(
                &RobotCommand::MoveJoints {
                    positions: BTreeMap::new()
                },
                &RobotState::default()
            )
            .is_err());
    }
    #[test]
    fn rejects_invalid_limits() {
        let p = JointLimitPolicy {
            limits: BTreeMap::from([(
                String::from("x"),
                JointLimit {
                    min: 1.0,
                    max: 0.0,
                    max_step: 1.0,
                },
            )]),
            ..Default::default()
        };
        assert!(p
            .validate(&RobotCommand::Stop, &RobotState::default())
            .is_err());
    }
    #[test]
    fn rejects_nonfinite_limit() {
        let p = JointLimitPolicy {
            limits: BTreeMap::from([(
                String::from("x"),
                JointLimit {
                    min: f64::NAN,
                    max: 1.0,
                    max_step: 1.0,
                },
            )]),
            ..Default::default()
        };
        assert!(p
            .validate(&RobotCommand::Stop, &RobotState::default())
            .is_err());
    }
    #[test]
    fn rejects_missing_battery() {
        let p = JointLimitPolicy {
            minimum_battery_percent: Some(1.0),
            ..Default::default()
        };
        assert!(p
            .validate(&RobotCommand::Stop, &RobotState::default())
            .is_err());
    }
    #[test]
    fn rejects_nonfinite_state() {
        let p = JointLimitPolicy::default();
        let s = RobotState {
            joints: BTreeMap::from([(String::from("x"), f64::NAN)]),
            ..Default::default()
        };
        assert!(p.validate(&RobotCommand::Stop, &s).is_err());
    }
    #[test]
    fn rejects_nonfinite_tolerance() {
        assert!(verify_target(&RobotCommand::Stop, &RobotState::default(), f64::NAN).is_err());
    }
    #[test]
    fn serde_rejects_unknown_fields() {
        assert!(
            serde_json::from_value::<RobotCommand>(json!({"command":"stop","extra":1})).is_err()
        );
    }
    #[tokio::test]
    async fn status_tracks_ack() {
        let h = harness();
        assert_eq!(h.status().await, RobotStatus::Ready);
        h.emergency_stop().await.unwrap();
        assert_eq!(h.status().await, RobotStatus::StopAcknowledged);
    }

    /// Driver boundary used by the lifecycle races below.  Every wait is
    /// released by a published signal; these tests deliberately contain no
    /// scheduler sleeps.
    struct ControlledDriver {
        state: Mutex<RobotState>,
        observe_started: Notify,
        release_observe: Notify,
        execute_started: Notify,
        release_execute: Notify,
        block_observe: AtomicBool,
        block_execute: AtomicBool,
        reject_execute: AtomicBool,
        skip_motion: AtomicBool,
        fail_stop: AtomicBool,
        calls: Mutex<Vec<RobotCommand>>,
        stop_calls: Mutex<usize>,
        stop_completed: Notify,
        stop_started: Notify,
        release_stop: Notify,
        block_stop: AtomicBool,
    }

    impl ControlledDriver {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(RobotState {
                    joints: BTreeMap::from([(String::from("shoulder"), 0.0)]),
                    battery_percent: Some(80.0),
                    ..Default::default()
                }),
                observe_started: Notify::new(),
                release_observe: Notify::new(),
                execute_started: Notify::new(),
                release_execute: Notify::new(),
                block_observe: AtomicBool::new(true),
                block_execute: AtomicBool::new(true),
                reject_execute: AtomicBool::new(false),
                skip_motion: AtomicBool::new(false),
                fail_stop: AtomicBool::new(false),
                calls: Mutex::new(Vec::new()),
                stop_calls: Mutex::new(0),
                stop_completed: Notify::new(),
                stop_started: Notify::new(),
                release_stop: Notify::new(),
                block_stop: AtomicBool::new(false),
            })
        }
    }

    #[async_trait]
    impl RobotDriver for ControlledDriver {
        async fn observe(&self) -> Result<RobotState> {
            self.observe_started.notify_one();
            if self.block_observe.swap(false, Ordering::SeqCst) {
                self.release_observe.notified().await;
            }
            Ok(self.state.lock().unwrap().clone())
        }

        async fn execute(&self, command: RobotCommand) -> Result<CommandReceipt> {
            self.calls.lock().unwrap().push(command.clone());
            self.execute_started.notify_one();
            if self.block_execute.swap(false, Ordering::SeqCst) {
                self.release_execute.notified().await;
            }
            let accepted = !self.reject_execute.load(Ordering::SeqCst);
            if accepted && !self.skip_motion.load(Ordering::SeqCst) {
                if let RobotCommand::MoveJoint { joint, position } = command {
                    self.state.lock().unwrap().joints.insert(joint, position);
                }
            }
            Ok(CommandReceipt {
                accepted,
                message: if accepted {
                    "accepted"
                } else {
                    "controller rejected"
                }
                .into(),
                metadata: Value::Null,
            })
        }

        async fn stop(&self) -> Result<()> {
            *self.stop_calls.lock().unwrap() += 1;
            self.stop_started.notify_one();
            if self.block_stop.load(Ordering::SeqCst) {
                self.release_stop.notified().await;
            }
            if self.fail_stop.load(Ordering::SeqCst) {
                return Err(Error::Tool("synthetic stop failure".into()));
            }
            self.release_execute.notify_waiters();
            self.release_observe.notify_waiters();
            self.stop_completed.notify_one();
            Ok(())
        }
    }

    fn controlled_harness(driver: Arc<ControlledDriver>) -> RobotHarness {
        RobotHarness::new(
            driver,
            Arc::new(JointLimitPolicy {
                limits: BTreeMap::from([(
                    String::from("shoulder"),
                    JointLimit {
                        min: -1.0,
                        max: 1.0,
                        max_step: 0.25,
                    },
                )]),
                minimum_battery_percent: Some(10.0),
            }),
        )
    }

    struct ScriptedModel {
        response: Mutex<Option<ModelResponse>>,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl Model for ScriptedModel {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
            *self.calls.lock().unwrap() += 1;
            Ok(self
                .response
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| ModelResponse::text("unexpected second model call")))
        }
    }

    fn move_response(id: &str, target: f64) -> ModelResponse {
        ModelResponse {
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "robot_command".into(),
                arguments: json!({
                    "command": "move_joint",
                    "joint": "shoulder",
                    "position": target,
                }),
            }],
            finish_reason: FinishReason::ToolCall,
            ..Default::default()
        }
    }

    fn two_move_response() -> ModelResponse {
        let mut response = move_response("move-1", 0.1);
        response.tool_calls.push(ToolCall {
            id: "move-2".into(),
            name: "robot_command".into(),
            arguments: json!({
                "command": "move_joint",
                "joint": "shoulder",
                "position": 0.2,
            }),
        });
        response
    }

    #[tokio::test]
    async fn public_tool_active_drop_stops_and_agent_loop_does_not_reenter() {
        let driver = ControlledDriver::new();
        let harness = controlled_harness(driver.clone());
        let mut tools = unravel_agent_runtime::ToolRegistry::new();
        harness.register_tools(&mut tools).unwrap();
        let model = Arc::new(ScriptedModel {
            response: Mutex::new(Some(two_move_response())),
            calls: Mutex::new(0),
        });
        let agent = AgentLoop::new(model.clone(), tools, "robot test");
        let stop = StopToken::new();
        let stop_for_run = stop.clone();
        let run = tokio::spawn(async move {
            let mut session = Session::new("active-drop");
            agent
                .run(&mut session, "move", &NoopEventSink, &stop_for_run)
                .await
        });
        driver.release_observe.notify_one();
        driver.execute_started.notified().await;
        stop.stop();
        let result = run.await.unwrap();
        assert!(matches!(result, Err(Error::Stopped)));
        assert!(*driver.stop_calls.lock().unwrap() >= 1);
        assert!(matches!(harness.status().await, RobotStatus::Fault(_)));
        assert_eq!(driver.calls.lock().unwrap().len(), 1);
        assert_eq!(*model.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn command_started_during_pending_stop_is_rejected_without_execute() {
        let driver = ControlledDriver::new();
        driver.block_stop.store(true, Ordering::SeqCst);
        let harness = controlled_harness(driver.clone());
        let stop = tokio::spawn({
            let harness = harness.clone();
            async move { harness.emergency_stop().await }
        });
        driver.stop_started.notified().await;

        let mut registry = unravel_agent_runtime::ToolRegistry::new();
        harness.register_tools(&mut registry).unwrap();
        let tool = registry.get("robot_command").unwrap();
        let result = tool
            .execute(json!({
                "command": "move_joint", "joint": "shoulder", "position": 0.1
            }))
            .await;
        assert!(result.is_err());
        assert!(driver.calls.lock().unwrap().is_empty());

        driver.release_stop.notify_one();
        assert!(stop.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn dropping_stop_caller_leaves_owned_stop_cleanup() {
        let driver = ControlledDriver::new();
        driver.block_stop.store(true, Ordering::SeqCst);
        let harness = controlled_harness(driver.clone());
        let stop = tokio::spawn({
            let harness = harness.clone();
            async move { harness.emergency_stop().await }
        });
        driver.stop_started.notified().await;
        stop.abort();
        driver.release_stop.notify_one();
        driver.stop_completed.notified().await;
        assert_eq!(harness.status().await, RobotStatus::StopAcknowledged);
    }

    #[tokio::test]
    async fn invalid_tolerance_is_rejected_before_execute() {
        let driver = ControlledDriver::new();
        let harness = controlled_harness(driver.clone()).with_postcondition_tolerance(f64::NAN);
        let mut registry = unravel_agent_runtime::ToolRegistry::new();
        harness.register_tools(&mut registry).unwrap();
        let tool = registry.get("robot_command").unwrap();
        driver.release_observe.notify_one();
        let result = tool
            .execute(json!({
                "command": "move_joint", "joint": "shoulder", "position": 0.1
            }))
            .await;
        assert!(matches!(result, Err(Error::Policy(_))));
        assert!(driver.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stop_during_blocked_observe_cannot_execute() {
        let driver = ControlledDriver::new();
        let harness = controlled_harness(driver.clone());
        let mut registry = unravel_agent_runtime::ToolRegistry::new();
        harness.register_tools(&mut registry).unwrap();
        let tool = registry.get("robot_command").unwrap();
        let command = tokio::spawn(async move {
            tool.execute(json!({
                "command": "move_joint", "joint": "shoulder", "position": 0.1
            }))
            .await
        });
        driver.observe_started.notified().await;
        harness.emergency_stop().await.unwrap();
        assert!(matches!(command.await.unwrap(), Err(Error::Stopped)));
        assert!(driver.calls.lock().unwrap().is_empty());
        assert_eq!(harness.status().await, RobotStatus::StopAcknowledged);
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_observe_deadline_faults_and_stops_without_sleeping() {
        let driver = ControlledDriver::new();
        let harness =
            controlled_harness(driver.clone()).with_observe_timeout(Duration::from_secs(5));
        let mut registry = unravel_agent_runtime::ToolRegistry::new();
        harness.register_tools(&mut registry).unwrap();
        let tool = registry.get("robot_command").unwrap();
        let command = tokio::spawn(async move {
            tool.execute(json!({
                "command": "move_joint", "joint": "shoulder", "position": 0.1
            }))
            .await
        });
        driver.observe_started.notified().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(command.await.unwrap(), Err(Error::Tool(_))));
        assert!(matches!(harness.status().await, RobotStatus::Fault(_)));
        assert!(driver.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_stop_keeps_reset_denied() {
        let driver = ControlledDriver::new();
        driver.fail_stop.store(true, Ordering::SeqCst);
        let harness = controlled_harness(driver);
        assert!(harness.emergency_stop().await.is_err());
        assert!(matches!(harness.status().await, RobotStatus::Fault(_)));
        assert!(matches!(
            harness.reset_emergency_stop().await,
            Err(Error::Policy(_))
        ));
    }

    #[tokio::test]
    async fn reset_and_cleanup_complete_without_lock_deadlock() {
        let driver = ControlledDriver::new();
        let harness = controlled_harness(driver.clone());
        let mut registry = unravel_agent_runtime::ToolRegistry::new();
        harness.register_tools(&mut registry).unwrap();
        let tool = registry.get("robot_command").unwrap();
        let command = tokio::spawn(async move {
            tool.execute(json!({
                "command": "move_joint", "joint": "shoulder", "position": 0.1
            }))
            .await
        });
        driver.release_observe.notify_one();
        driver.execute_started.notified().await;
        let reset_harness = harness.clone();
        let reset = tokio::spawn(async move { reset_harness.reset_emergency_stop().await });
        command.abort();
        driver.stop_completed.notified().await;
        assert!(reset.await.unwrap().is_ok());
        assert_eq!(harness.status().await, RobotStatus::Ready);
    }

    #[tokio::test]
    async fn concurrent_registered_tools_take_fresh_serial_observations() {
        let driver = ControlledDriver::new();
        let harness = controlled_harness(driver.clone());
        let mut registry = unravel_agent_runtime::ToolRegistry::new();
        harness.register_tools(&mut registry).unwrap();
        let first = registry.get("robot_command").unwrap();
        let second = registry.get("robot_command").unwrap();
        let a = tokio::spawn(async move {
            first
                .execute(json!({"command":"move_joint","joint":"shoulder","position":0.1}))
                .await
        });
        driver.observe_started.notified().await;
        let b = tokio::spawn(async move {
            second
                .execute(json!({"command":"move_joint","joint":"shoulder","position":0.2}))
                .await
        });
        driver.release_observe.notify_waiters();
        driver.execute_started.notified().await;
        driver.release_execute.notify_waiters();
        assert!(a.await.unwrap().is_ok());
        assert!(b.await.unwrap().is_ok());
        assert_eq!(driver.calls.lock().unwrap().len(), 2);
    }

    fn small_move() -> RobotCommand {
        RobotCommand::MoveJoint {
            joint: "shoulder".into(),
            position: 0.1,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn queued_command_expires_without_dispatch_or_stopping_active_motion() {
        let driver = ControlledDriver::new();
        driver.block_observe.store(false, Ordering::SeqCst);
        let harness = controlled_harness(driver.clone())
            .with_queue_timeout(Duration::from_secs(1))
            .with_execute_timeout(Duration::from_secs(30));
        let active = tokio::spawn({
            let harness = harness.clone();
            async move { harness.execute(small_move()).await }
        });
        driver.execute_started.notified().await;
        let error = harness.execute(small_move()).await.unwrap_err();
        assert!(error.to_string().contains("queue timed out"));
        assert_eq!(driver.calls.lock().unwrap().len(), 1);
        assert_eq!(*driver.stop_calls.lock().unwrap(), 0);
        driver.release_execute.notify_one();
        assert!(active.await.unwrap().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_queue_deadline_latches_fault_without_dispatch() {
        let driver = ControlledDriver::new();
        let harness = controlled_harness(driver.clone());
        let _held_stop = harness.coordinator.stop_gate.lock().await;
        harness.cleanup_fault("uncertain command".into()).await;
        assert!(matches!(harness.status().await, RobotStatus::Fault(reason)
            if reason.contains("uncertain command") && reason.contains("cleanup stop queue timed out")));
        assert!(harness.execute(small_move()).await.is_err());
        assert!(driver.calls.lock().unwrap().is_empty());
        assert!(!harness.coordinator.lifecycle.lock().await.stop_acknowledged);
    }

    #[tokio::test(start_paused = true)]
    async fn execute_timeout_and_stop_timeout_remain_faulted_without_retry() {
        let driver = ControlledDriver::new();
        driver.block_observe.store(false, Ordering::SeqCst);
        driver.block_stop.store(true, Ordering::SeqCst);
        let harness = controlled_harness(driver.clone());
        let start = tokio::time::Instant::now();
        assert!(harness.execute(small_move()).await.is_err());
        assert!(start.elapsed() <= Duration::from_secs(7));
        assert!(matches!(harness.status().await, RobotStatus::Fault(reason)
            if reason.contains("execute timed out") && reason.contains("cleanup stop timed out")));
        assert!(harness.reset_emergency_stop().await.is_err());
        assert!(harness.execute(small_move()).await.is_err());
        assert_eq!(driver.calls.lock().unwrap().len(), 1);
        assert_eq!(*driver.stop_calls.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn verification_timeout_stops_and_requires_explicit_reset() {
        let driver = ControlledDriver::new();
        driver.block_observe.store(false, Ordering::SeqCst);
        let harness = controlled_harness(driver.clone());
        let command = tokio::spawn({
            let harness = harness.clone();
            async move { harness.execute(small_move()).await }
        });
        driver.execute_started.notified().await;
        driver.block_observe.store(true, Ordering::SeqCst);
        driver.release_execute.notify_one();
        assert!(command.await.unwrap().is_err());
        assert!(matches!(harness.status().await, RobotStatus::Fault(reason)
            if reason.contains("post-action observation failed")));
        assert_eq!(*driver.stop_calls.lock().unwrap(), 1);
        assert!(harness.execute(small_move()).await.is_err());
        harness.reset_emergency_stop().await.unwrap();
        assert_eq!(harness.status().await, RobotStatus::Ready);
    }

    #[tokio::test]
    async fn rejected_receipt_is_not_a_successful_tool_result() {
        let driver = ControlledDriver::new();
        driver.block_observe.store(false, Ordering::SeqCst);
        driver.block_execute.store(false, Ordering::SeqCst);
        driver.reject_execute.store(true, Ordering::SeqCst);
        let harness = controlled_harness(driver.clone());
        let mut registry = ToolRegistry::new();
        harness.register_tools(&mut registry).unwrap();
        let error = registry
            .get("robot_command")
            .unwrap()
            .execute(serde_json::to_value(small_move()).unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("controller rejected"));
        assert_eq!(driver.state.lock().unwrap().joints["shoulder"], 0.0);
    }

    #[tokio::test]
    async fn acknowledged_but_unreached_target_faults_and_reports_stop_failure() {
        let driver = ControlledDriver::new();
        driver.block_observe.store(false, Ordering::SeqCst);
        driver.block_execute.store(false, Ordering::SeqCst);
        driver.skip_motion.store(true, Ordering::SeqCst);
        driver.fail_stop.store(true, Ordering::SeqCst);
        let harness = controlled_harness(driver.clone());
        assert!(harness.execute(small_move()).await.is_err());
        assert!(matches!(harness.status().await, RobotStatus::Fault(reason)
            if reason.contains("verification failed") && reason.contains("synthetic stop failure")));
        assert!(harness.reset_emergency_stop().await.is_err());
        assert!(harness.execute(small_move()).await.is_err());
        assert_eq!(driver.calls.lock().unwrap().len(), 1);
    }
}
