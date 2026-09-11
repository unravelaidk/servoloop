pub(crate) struct JournalTool {
    pub(crate) inner: Arc<dyn Tool>,
    pub(crate) guard: Arc<SessionGuard>,
    pub(crate) intent: JournalRecord,
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
        intent.intent_id = new_id("intent");
        intent.arguments = redact_value(args.clone()).map_err(servoloop_core::Error::Tool)?;
        self.guard.append(intent.clone()).map_err(|e| {
            servoloop_core::Error::Tool(format!("journal failed; motion not dispatched: {e}"))
        })?;
        match self.inner.execute(args).await {
            Ok(out) => {
                let mut done = intent;
                done.kind = "result".into();
                done.outcome = Some("verified".into());
                self.guard
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
                let _ = self.guard.append(failed);
                Err(e)
            }
        }
    }
}

use crate::output::redact_value;
use async_trait::async_trait;
use serde_json::Value;
use servoloop_core::{Result as CoreResult, Tool, ToolOutput};
use servoloop_store::{new_id, JournalRecord, SessionGuard};
use std::sync::Arc;
