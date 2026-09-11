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

- A model-agnostic agent loop with bounded turns and retries.
- A typed registry for JSON-schema tools.
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

## Design principles

ServoLoop uses the following constraints by default:

1. Observe the current state before validating every command.
2. Execute physical commands sequentially, not concurrently.
3. Reject unknown joints and non-finite numeric values.
4. Bound joint positions, per-command motion, model turns, retries, and time.
5. Keep the model, robot driver, and safety policy independently replaceable.
6. Record structured events around every model and tool operation.

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
