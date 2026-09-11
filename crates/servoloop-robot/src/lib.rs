//! Hardware-neutral robot control with mandatory safety checks.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use servoloop_core::{Error, Result, Tool, ToolDefinition, ToolOutput, ToolRegistry};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RobotState {
    pub joints: BTreeMap<String, f64>,
    pub battery_percent: Option<f64>,
    pub emergency_stop: bool,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum RobotCommand {
    MoveJoint { joint: String, position: f64 },
    MoveJoints { positions: BTreeMap<String, f64> },
    SetOutput { channel: String, value: f64 },
    Stop,
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
        if state.emergency_stop && !matches!(command, RobotCommand::Stop) {
            return Err(Error::Safety("the robot reports an emergency stop".into()));
        }
        if let (Some(minimum), Some(actual)) = (self.minimum_battery_percent, state.battery_percent)
        {
            if actual < minimum && !matches!(command, RobotCommand::Stop) {
                return Err(Error::Safety(format!(
                    "battery is {actual:.1}%, below the {minimum:.1}% minimum"
                )));
            }
        }

        match command {
            RobotCommand::MoveJoint { joint, position } => {
                self.validate_joint(joint, *position, state)
            }
            RobotCommand::MoveJoints { positions } => {
                for (joint, position) in positions {
                    self.validate_joint(joint, *position, state)?;
                }
                Ok(())
            }
            RobotCommand::SetOutput { value, .. } if !value.is_finite() => {
                Err(Error::Safety("output value must be finite".into()))
            }
            RobotCommand::SetOutput { .. } | RobotCommand::Stop => Ok(()),
        }
    }
}

impl JointLimitPolicy {
    fn validate_joint(&self, joint: &str, target: f64, state: &RobotState) -> Result<()> {
        if !target.is_finite() {
            return Err(Error::Safety(format!(
                "joint `{joint}` target must be finite"
            )));
        }
        let limit = self
            .limits
            .get(joint)
            .ok_or_else(|| Error::Safety(format!("joint `{joint}` is not configured")))?;
        if !(limit.min..=limit.max).contains(&target) {
            return Err(Error::Safety(format!(
                "joint `{joint}` target {target} is outside [{}, {}]",
                limit.min, limit.max
            )));
        }
        let current = state
            .joints
            .get(joint)
            .ok_or_else(|| Error::Safety(format!("joint `{joint}` has no observed position")))?;
        if (target - current).abs() > limit.max_step {
            return Err(Error::Safety(format!(
                "joint `{joint}` move exceeds the maximum step of {}",
                limit.max_step
            )));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct RobotHarness {
    driver: Arc<dyn RobotDriver>,
    policy: Arc<dyn SafetyPolicy>,
    emergency_stop: Arc<AtomicBool>,
    command_timeout: Duration,
}

impl RobotHarness {
    pub fn new(driver: Arc<dyn RobotDriver>, policy: Arc<dyn SafetyPolicy>) -> Self {
        Self {
            driver,
            policy,
            emergency_stop: Arc::new(AtomicBool::new(false)),
            command_timeout: Duration::from_secs(5),
        }
    }

    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    pub async fn emergency_stop(&self) -> Result<()> {
        self.emergency_stop.store(true, Ordering::SeqCst);
        self.driver.stop().await
    }

    pub fn reset_emergency_stop(&self) {
        self.emergency_stop.store(false, Ordering::SeqCst);
    }

    pub fn register_tools(&self, registry: &mut ToolRegistry) -> Result<()> {
        registry.register(RobotObserveTool(self.clone()))?;
        registry.register(RobotCommandTool(self.clone()))?;
        Ok(())
    }

    async fn execute(&self, command: RobotCommand) -> Result<CommandReceipt> {
        if self.emergency_stop.load(Ordering::SeqCst) && !matches!(command, RobotCommand::Stop) {
            return Err(Error::Safety("ServoLoop emergency stop is engaged".into()));
        }
        let state = self.driver.observe().await?;
        self.policy.validate(&command, &state)?;
        if matches!(command, RobotCommand::Stop) {
            self.driver.stop().await?;
            return Ok(CommandReceipt {
                accepted: true,
                message: "robot stopped".into(),
                metadata: Value::Null,
            });
        }
        tokio::time::timeout(self.command_timeout, self.driver.execute(command))
            .await
            .map_err(|_| Error::Tool("robot command timed out".into()))?
    }
}

struct RobotObserveTool(RobotHarness);

#[async_trait]
impl Tool for RobotObserveTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "robot_observe".into(),
            description: "Read the robot state before deciding on a command.".into(),
            parameters: json!({"type": "object", "additionalProperties": false}),
        }
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutput> {
        let state = self.0.driver.observe().await?;
        Ok(ToolOutput::with_metadata(
            serde_json::to_string(&state).map_err(|error| Error::Tool(error.to_string()))?,
            serde_json::to_value(state).map_err(|error| Error::Tool(error.to_string()))?,
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
            parameters: json!({
                "oneOf": [
                    {"type": "object", "properties": {"command": {"const": "move_joint"}, "joint": {"type": "string"}, "position": {"type": "number"}}, "required": ["command", "joint", "position"]},
                    {"type": "object", "properties": {"command": {"const": "move_joints"}, "positions": {"type": "object", "additionalProperties": {"type": "number"}}}, "required": ["command", "positions"]},
                    {"type": "object", "properties": {"command": {"const": "set_output"}, "channel": {"type": "string"}, "value": {"type": "number"}}, "required": ["command", "channel", "value"]},
                    {"type": "object", "properties": {"command": {"const": "stop"}}, "required": ["command"]}
                ]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput> {
        let command: RobotCommand = serde_json::from_value(arguments)
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        let receipt = self.0.execute(command).await?;
        Ok(ToolOutput::with_metadata(
            receipt.message.clone(),
            serde_json::to_value(receipt).map_err(|error| Error::Tool(error.to_string()))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeDriver(Mutex<RobotState>);

    #[async_trait]
    impl RobotDriver for FakeDriver {
        async fn observe(&self) -> Result<RobotState> {
            Ok(self.0.lock().unwrap().clone())
        }

        async fn execute(&self, command: RobotCommand) -> Result<CommandReceipt> {
            if let RobotCommand::MoveJoint { joint, position } = command {
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
        let driver = Arc::new(FakeDriver(Mutex::new(RobotState {
            joints: BTreeMap::from([("shoulder".into(), 0.0)]),
            battery_percent: Some(80.0),
            ..RobotState::default()
        })));
        let policy = Arc::new(JointLimitPolicy {
            limits: BTreeMap::from([(
                "shoulder".into(),
                JointLimit {
                    min: -1.0,
                    max: 1.0,
                    max_step: 0.25,
                },
            )]),
            minimum_battery_percent: Some(10.0),
        });
        RobotHarness::new(driver, policy)
    }

    #[tokio::test]
    async fn accepts_a_move_within_limits() {
        let receipt = harness()
            .execute(RobotCommand::MoveJoint {
                joint: "shoulder".into(),
                position: 0.2,
            })
            .await
            .unwrap();
        assert!(receipt.accepted);
    }

    #[tokio::test]
    async fn rejects_a_move_that_is_too_large() {
        let error = harness()
            .execute(RobotCommand::MoveJoint {
                joint: "shoulder".into(),
                position: 0.5,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Safety(_)));
    }

    #[tokio::test]
    async fn emergency_stop_blocks_motion() {
        let harness = harness();
        harness.emergency_stop().await.unwrap();
        let error = harness
            .execute(RobotCommand::MoveJoint {
                joint: "shoulder".into(),
                position: 0.1,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Safety(_)));
    }
}
