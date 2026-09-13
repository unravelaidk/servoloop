pub(crate) struct SimulatedDriver(pub(crate) Mutex<RobotState>);
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

pub(crate) struct DemoModel(pub(crate) Mutex<u8>);
#[async_trait]
impl Model for DemoModel {
    async fn complete(&self, request: ModelRequest) -> CoreResult<ModelResponse> {
        let mut n = self.0.lock().await;
        let suffix = request.messages.len();
        let response = match *n {
            0 => ModelResponse {
                content: "Inspecting first.".into(),
                tool_calls: vec![ToolCall {
                    id: format!("observe-{suffix}"),
                    name: "robot_observe".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
            1 => ModelResponse {
                content: "Making the requested small move.".into(),
                tool_calls: vec![ToolCall {
                    id: format!("move-{suffix}"),
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

use async_trait::async_trait;
use serde_json::{json, Value};
use servoloop_robot::{CommandReceipt, RobotCommand, RobotDriver, RobotState};
use tokio::sync::Mutex;
use unravel_agent_runtime::{Model, ModelRequest, ModelResponse, Result as CoreResult, ToolCall};

use servoloop_robot::{JointLimit, JointLimitPolicy, RobotHarness};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) fn harness() -> RobotHarness {
    let driver = Arc::new(SimulatedDriver(Mutex::new(RobotState {
        joints: BTreeMap::from([(String::from("shoulder"), 0.0)]),
        battery_percent: Some(100.0),
        ..Default::default()
    })));
    RobotHarness::new(
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
    )
}
