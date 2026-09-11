use async_trait::async_trait;
use serde_json::{json, Value};
use servoloop_core::{
    AgentLoop, Error, Event, Model, ModelRequest, ModelResponse, Result, Session, StopToken,
    ToolCall, ToolRegistry,
};
use servoloop_robot::{
    CommandReceipt, JointLimit, JointLimitPolicy, RobotCommand, RobotDriver, RobotHarness,
    RobotState,
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;

struct SimulatedArm(Mutex<RobotState>);

#[async_trait]
impl RobotDriver for SimulatedArm {
    async fn observe(&self) -> Result<RobotState> {
        Ok(self.0.lock().await.clone())
    }

    async fn execute(&self, command: RobotCommand) -> Result<CommandReceipt> {
        if let RobotCommand::MoveJoint { joint, position } = command {
            self.0.lock().await.joints.insert(joint, position);
        }
        Ok(CommandReceipt {
            accepted: true,
            message: "simulator accepted command".into(),
            metadata: Value::Null,
        })
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}

struct DemoModel(Mutex<usize>);

#[async_trait]
impl Model for DemoModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        let mut turn = self.0.lock().await;
        let response = match *turn {
            0 => ModelResponse {
                content: "I will inspect the robot first.".into(),
                tool_calls: vec![ToolCall {
                    id: "observe-1".into(),
                    name: "robot_observe".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
            1 => ModelResponse {
                content: "The requested move is within the configured step limit.".into(),
                tool_calls: vec![ToolCall {
                    id: "move-1".into(),
                    name: "robot_command".into(),
                    arguments: json!({
                        "command": "move_joint",
                        "joint": "shoulder",
                        "position": 0.2
                    }),
                }],
                ..Default::default()
            },
            2 => ModelResponse::text("The simulated shoulder moved to 0.2 radians."),
            _ => return Err(Error::Model("demo script exhausted".into())),
        };
        *turn += 1;
        Ok(response)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let driver = Arc::new(SimulatedArm(Mutex::new(RobotState {
        joints: BTreeMap::from([("shoulder".into(), 0.0)]),
        battery_percent: Some(100.0),
        ..RobotState::default()
    })));
    let policy = Arc::new(JointLimitPolicy {
        limits: BTreeMap::from([(
            "shoulder".into(),
            JointLimit {
                min: -1.5,
                max: 1.5,
                max_step: 0.25,
            },
        )]),
        minimum_battery_percent: Some(10.0),
    });
    let harness = RobotHarness::new(driver, policy);
    let mut tools = ToolRegistry::new();
    harness.register_tools(&mut tools)?;

    let agent = AgentLoop::new(
        Arc::new(DemoModel(Mutex::new(0))),
        tools,
        "You control a robot. Observe before acting and use small, verifiable motions.",
    );
    let mut session = Session::new("simulated-arm-demo");
    let output = agent
        .run(
            &mut session,
            "Move the shoulder to 0.2 radians.",
            &|event: Event| println!("{}", serde_json::to_string(&event).unwrap()),
            &StopToken::default(),
        )
        .await?;
    println!("\n{output}");
    Ok(())
}
