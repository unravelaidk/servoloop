# ServoLoop

ServoLoop is an open-source Rust harness for connecting agentic control loops
to robots. It keeps model providers and robot hardware behind small traits, so
you can test control behavior in simulation before connecting physical devices.

<!-- prettier-ignore -->
> [!WARNING]
> ServoLoop is experimental. Don't connect it to hardware that can injure people
> or damage property without independent, hardware-level safety systems.

## What it provides

ServoLoop starts with the reusable control-loop ideas developed in Arvel and
adapts them for embodied systems:

- A model-agnostic agent loop with bounded turns, classified retries, and
  cancellable model/tool operations.
- A provider-neutral `Model` trait with optional streaming, typed usage/finish
  metadata, and sampling parameters.
- A typed registry for JSON-schema tools with batch-validated, unique tool-call
  IDs.
- Multimodal message content (text and image) with an ergonomic text-only path.
- Conservative interrupted-session reconciliation: tool calls without results
  become explicit unknown outcomes, never an implied success or actuator
  replay. The loop **halts** when any unresolved unknown exists; the caller
  must replace it with a resolved result before resuming.
- Tool calls validated across the full session history (unique non-empty IDs,
  registered names, no reuse of prior-turn IDs) before any side effects.
- Non-dispatchable finish reasons (max output, content filter) are rejected
  so the loop never acts on truncated or filtered output.
- Ordered, sequential command execution for predictable robot state.
- Structured lifecycle events for logs, telemetry, and user interfaces.
- Hardware-neutral robot driver and safety policy traits.
- Joint limits, maximum motion steps, command timeouts, and emergency stops.
- A deterministic simulated-arm example that doesn't require an API key.

## Repository layout

The workspace separates the generic agent runtime from robot-specific code:

- `crates/servoloop-core` contains the agent loop, model trait, tools, sessions,
  events, retries, and stop token.
- `crates/servoloop-robot` contains robot drivers, commands, state, safety
  policies, and agent-facing robot tools.
- `examples/simulated-arm` demonstrates an observe-act-observe loop without
  physical hardware.

## Run the example

Run the deterministic simulator from the repository root:

```bash
cargo run -p servoloop-simulated-arm
```

The example emits newline-delimited JSON events, executes a safety-checked
joint command, and prints the final model response.

## Implement a model provider

The `Model` trait is the only contract a provider implements. Implement
`complete` to return a fully assembled response; override `stream` and set
`can_stream` to `true` when the provider supports streaming deltas. The loop
assembles all streamed deltas into a single `ModelResponse` before any tool
executes, so streaming and non-streaming providers are interchangeable.

```rust
use servoloop_core::{Error, Model, ModelError, ModelRequest, ModelResponse, Result};
use async_trait::async_trait;

struct MyModel;

#[async_trait]
impl Model for MyModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        // Forward to your provider, classify errors, and return a complete
        // response. Use ModelError::auth / invalid for permanent errors and
        // ModelError::transient / rate_limit for retryable ones.
        Err(Error::ModelTyped(ModelError::auth("not implemented")))
    }
}
```

The loop carries `session_id` and `max_output` through requests and typed
usage/finish metadata through responses. Sampling parameters are optional so a
provider that doesn't support a knob can ignore it.

## Implement a robot driver

Implement `RobotDriver` for your simulator, middleware, or hardware adapter:

```rust
#[async_trait]
impl RobotDriver for MyRobot {
    async fn observe(&self) -> Result<RobotState> {
        // Read sensors and return a fresh state snapshot.
    }

    async fn execute(&self, command: RobotCommand) -> Result<CommandReceipt> {
        // Translate a validated command to your robot protocol.
    }

    async fn stop(&self) -> Result<()> {
        // Trigger the fastest available software stop.
    }
}
```

Keep hardware emergency-stop circuits independent from ServoLoop. A software
stop can't replace a certified physical safety system.

## Cancellation

The `StopToken` (backed by `tokio::sync::watch`) lets the loop interrupt
pending model calls, retry backoff sleeps, and tool futures without a
lost-wakeup race. A clone of the token is passed to `AgentLoop::run`; calling
`stop()` signals all clones.

When a tool is cancelled or exceeds its deadline, the loop marks that tool
call as `ToolUnknown` in the session history and **halts immediately** — no
subsequent tools in the batch execute and no subsequent model turn runs. The
caller must reconcile the unknown outcome before resuming.

> [!IMPORTANT]
> Cancelling a tool future drops the Rust-side future but does **not** stop
> any external effect the tool has already dispatched (e.g. robot motion).
> Treat a cancelled tool as an unknown outcome and reconcile the physical
> state before resuming. Never assume cancelling the future halts motion.

## Design principles

ServoLoop uses the following constraints by default:

1. Observe the current state before validating every command.
2. Execute physical commands sequentially, not concurrently.
3. Reject unknown joints and non-finite numeric values.
4. Bound joint positions, per-command motion, model turns, retries, and time.
5. Keep the model, robot driver, and safety policy independently replaceable.
6. Record structured events around every model and tool operation.
7. Execute tools only after a complete, validated model response — never from
   partial streamed arguments, a cancelled response, or a failed stream.
8. Reconcile interrupted histories: tool calls without results become
   explicit unknown outcomes, never an implied success or actuator replay.
   The loop halts until the caller replaces each unknown with a resolved
   result.

## Development

Use the standard Rust checks before submitting a change:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

ServoLoop is licensed under the GNU Affero General Public License v3.0 only.
See `LICENSE` for the complete terms.