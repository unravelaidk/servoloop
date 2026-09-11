# Contributing to ServoLoop

ServoLoop is an experimental Rust harness for model-driven robot control. This
guide covers the checks that run without credentials, GPUs, Isaac Sim, or
physical hardware.

## Prerequisites

Install a current stable Rust toolchain with `rustfmt` and `clippy`. The
minimum Rust version is 1.88: ICU dependencies in the lockfile require it,
and `cargo +1.88.0 check --workspace --all-targets --locked` passes on Linux.
CI checks this minimum on Linux and runs stable Rust tests on Linux, Windows,
and macOS. This host matrix does not imply Isaac Sim support on those hosts.

The workspace lockfile is checked in. Use `--locked` for reproducible Cargo
commands and do not update `Cargo.lock` as part of an unrelated change.

## Local checks

Run these commands from the repository root:

```bash
cargo fmt --all -- --check
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo doc --workspace --no-deps --locked
```

The offline deterministic smoke test does not contact a model provider:

```bash
cargo run --locked -p servoloop-simulated-arm
```

The `servoloop-openai-compatible-demo` example is a live-provider example. It
requires an API key and makes network requests, so it is not part of standard
CI. Never add a real key to a commit or to CI logs.

## Provider and discovery setup

The provider crate implements OpenAI-compatible Chat Completions. Built-in
provider specifications cover OpenAI (`OPENAI_API_KEY`), NVIDIA
(`NVIDIA_API_KEY`), OpenRouter (`OPENROUTER_API_KEY`), and local Ollama
(`http://localhost:11434/v1`). Explicit keys and base URLs take precedence
over environment variables.

Capability metadata can be discovered from the Models.dev catalog at
`https://models.dev/api.json`, or from a provider's local `/models` endpoint.
Models.dev metadata describes capabilities; it does not prove that invocation
will succeed. Use `DiscoveryOptions { include_models_dev: false, ..Default::default() }`
when an offline or local-only discovery operation must not fetch Models.dev.

## Simulation, Isaac, and hardware

The simulated-arm example is an offline demonstration of the control-loop
contracts. It is not evidence that a model provider, Isaac Sim, a robot, or a
physical safety system works.

Isaac Sim integration is pending a selected Isaac version, robot and scene,
deployment topology, NVIDIA GPU/runtime requirements, and adapter contract.
No Isaac CLI is installed or invoked by this repository, and no Isaac job is
reported as passing. Record any future verification as a separate, explicitly
provisioned manual or opt-in integration procedure.

ServoLoop software stops and cancellation are not safety-rated physical
protection. A cancelled or timed-out operation can have an external effect
already in flight and is therefore recorded as an unknown outcome. Reconcile
the physical state before resuming, and never automatically replay uncertain
motion.

## Release status

Release publication and binary packaging are blocked until the CLI/store
deliverable defines the binary name, flags, exit codes, fixtures, supported
targets, archive layout, and checksum procedure. Do not add credentials,
signing keys, or a publish workflow before those requirements are verified.

## Pull requests

Keep changes focused, explain the checks you ran, and identify checks that
could not run. Do not describe optional simulator or hardware checks as
passed. Changes to shared workspace manifests and the lockfile require
coordination with the primary integrator.
