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

The workspace keeps robot-specific code local and pins the shared Rust packages
from [Unravel Agent Runtime](https://github.com/unravelaidk/unravel-agent-runtime)
to an exact Git revision in `Cargo.toml` and `Cargo.lock`:

- `unravel-agent-runtime` owns the agent loop, model/tool contracts, sessions,
  events, retries, and stop token.
- `unravel-agent-providers` owns Chat Completions transport and model discovery.
- `crates/servoloop-robot` contains robot drivers, commands, state, safety
  policies, and agent-facing robot tools.
- `crates/servoloop-store` owns durable sessions, intent/result journals, and leases.
- `crates/servoloop-isaac` owns the simulator bridge.
- `apps/servoloop-cli` owns CLI/TUI presentation and execution policy, including
  journal-before-side-effect ordering and application-specific provider headers.
- `examples/simulated-arm` demonstrates an observe-act-observe loop without
  physical hardware.

## Run the example

Run the deterministic simulator from the repository root:

```bash
cargo run -p servoloop-simulated-arm
```

The example emits newline-delimited JSON events, executes a safety-checked
joint command, and prints the final model response.

## Use a model provider

The shared provider crate implements OpenAI-compatible Chat Completions. For a
different protocol, implement the shared `Model` trait. Providers return a fully
assembled response; streaming events do not dispatch partial tool calls.

```rust,no_run
use unravel_agent_runtime::{Message, Model, ModelRequest};
use unravel_agent_providers::{OpenAiCompatProvider, ProviderSpec};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let spec = ProviderSpec::openai(std::env::var("OPENAI_API_KEY")?);
let provider = OpenAiCompatProvider::new(spec, "gpt-4o-mini")?;
let request = ModelRequest::new("demo", vec![Message::user_text("Hello")]);
let response = provider.complete(request).await?;
println!("{}", response.content);
# Ok(())
# }
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

## Install the CLI

The CLI packaging workflow uploads native, checksum accompanied artifacts for
Linux x86_64 (glibc 2.39), Windows x86_64, and macOS arm64 and x86_64. It does
not publish releases yet. See [`docs/install.md`](docs/install.md) for manual
artifact installation and verification.

For the interactive offline-demo workspace, run from a clone in a terminal:

```bash
cargo run --locked -p servoloop-cli -- ui
```

Press Enter to review the demo, then `r` to run it. No model credentials or
network connection are needed. See the [terminal UI guide](apps/servoloop-cli/README.md#interactive-terminal-workspace)
for controls, themes, storage, and current limitations.

From a clone, you can also install the current CLI into Cargo's user bin
directory:

```bash
cargo install --path apps/servoloop-cli --locked
```

This source install requires a clone; ServoLoop is not published on crates.io.

## Development

Use the standard Rust checks before submitting a change:

```bash
cargo fmt --all -- --check
cargo build --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo doc --workspace --no-deps --locked
```

The CI workflow runs these checks on stable Rust for Linux, Windows, and
macOS. It also runs the simulated-arm example as an offline deterministic
smoke test. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for provider setup,
Models.dev discovery, compatibility evidence, and hardware limitations.

The OpenAI-compatible provider supports OpenAI, NVIDIA, OpenRouter, local
Ollama, and custom endpoints. Provider credentials use the corresponding
environment variables, such as `OPENAI_API_KEY`; the live demo requires a
real key and is not run in CI. Models.dev capability metadata is discovery
input, not a guarantee that a model invocation succeeds.

Isaac Sim verification is not included in this repository's default CI. The
Isaac version, robot/scene, NVIDIA runtime, and deployment topology remain
pending an explicit adapter decision. No Isaac CLI is installed here. The
simulated example is not hardware validation, and software stop is not a
safety-rated physical safeguard.

## License

ServoLoop is licensed under the GNU Affero General Public License v3.0 only.
See `LICENSE` for the complete terms.
