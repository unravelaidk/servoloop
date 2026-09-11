# Isaac Sim bridge

This adapter exposes protocol version 1 over authenticated HTTP. The default
CPU backend is a deterministic mock and is not an Isaac integration test.
Every state-changing operation is queued to one owner thread; HTTP workers do
not call simulator APIs. Command IDs are deduplicated, and a disconnected or
timed-out command is never replayed automatically.

## Run the mock

```sh
export SERVOLOOP_BRIDGE_TOKEN='use-a-local-secret'
python3 adapters/isaac-sim/bridge.py
```

Use `http://127.0.0.1:8765` only for local development. Remote endpoints must
be protected by TLS and an explicitly configured reverse proxy; the bridge
refuses arbitrary non-loopback HTTP bindings. Tokens are sent in the
`Authorization` header and never logged or accepted on the command line.

Run the Python compatibility tests with:

```sh
cd adapters/isaac-sim && python3 -m unittest -v test_bridge.py
```

With the mock running in another terminal, run the cross-process Rust client:

```sh
export SERVOLOOP_BRIDGE_TOKEN='use-a-local-secret'
cargo run -p servoloop-isaac-example
```

It prints observations before and after one bounded command. The command
receipt is an acknowledgement from the bridge; production callers must use a
fresh observation (and the `RobotHarness` settling contract) before treating
motion as verified.

## Isaac setup (manual, unverified here)

The integration seam targets Isaac Sim 4.5 APIs documented by NVIDIA. Start
`SimulationApp` before Isaac imports, create/configure `World`, an
`Articulation`, its `ArticulationController`, and cameras, then construct
`IsaacBackend` and call `serve_backend(backend)` from the simulator process.
The backend uses `world.step(render=True)`, `world.reset()`, controller
`apply_action(ArticulationAction(...))`, and camera `get_rgba()`. Camera data
is encoded as bounded PNG by the bridge. Stop pauses the Isaac `World`; it is
only a simulation pause and is not a hardware safety guarantee.
Supply an operator-owned scene and configuration; clients cannot load USD or
choose arbitrary prims. This repository has no Isaac installation, GPU, scene,
or robot, so no simulator test is claimed.

The Rust `IsaacDriver` requires a non-empty token, performs protocol
negotiation, bounds responses to 4 MiB, validates loopback/HTTPS topology, and
implements `RobotDriver`. `RobotHarness` remains responsible for bounded
timeouts and post-command observation verification. `stop` is a software stop
request only; this project makes no hardware-safety claims.
