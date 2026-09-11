use servoloop_isaac::IsaacDriver;
use servoloop_robot::{RobotCommand, RobotDriver};
use std::{env, time::Duration};

/// Observe–act–verify against either the CPU mock or an operator-configured
/// Isaac bridge. This example never retries an ambiguous command.
#[tokio::main]
async fn main() -> servoloop_core::Result<()> {
    let endpoint =
        env::var("SERVOLOOP_BRIDGE_URL").unwrap_or_else(|_| "http://127.0.0.1:8765".into());
    let token = env::var("SERVOLOOP_BRIDGE_TOKEN").map_err(|_| {
        servoloop_core::Error::InvalidInput("SERVOLOOP_BRIDGE_TOKEN is required".into())
    })?;
    let driver = IsaacDriver::with_timeout(endpoint, token, Duration::from_secs(5))?;
    driver.handshake("operator-configured").await?;
    let before = driver.observe().await?;
    println!("before: {}", serde_json::to_string(&before).unwrap());
    driver
        .execute(RobotCommand::MoveJoint {
            joint: "shoulder".into(),
            position: 0.1,
        })
        .await?;
    let after = driver.observe().await?;
    println!("after: {}", serde_json::to_string(&after).unwrap());
    Ok(())
}
