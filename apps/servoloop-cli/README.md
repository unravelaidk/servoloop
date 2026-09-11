# ServoLoop CLI

`servoloop` emits NDJSON events on standard output for both `run --demo` and
live `run --prompt TEXT`, and `resume SESSION --prompt TEXT` executions;
diagnostics always go to standard error.
Live runs use the existing OpenAI-compatible provider and the simulated driver
in this slice. Ctrl-C cancels the model loop, awaits a bounded emergency-stop
cleanup, and never reports an interrupted action as successful. This driver
never represents hardware motion or an emergency stop as safety-rated.

Configuration is versioned JSON (`{"version":1}`), with command-line values
overriding environment values, then the selected config file, then defaults.
Credentials are read from provider-defined environment variables and are never
serialized by `config`. The store uses a private directory where supported and
syncs append-only journal records before dispatch. Completed runs save an
atomic session snapshot with a journal watermark. `resume` acquires the
session lease before loading the snapshot and journal, refuses unresolved or
stale state, and keeps the lease through the terminal snapshot write. Resumed
simulated runs use a fresh simulator; prior observations remain historical and
are not replayed. The store rejects symlink journal paths, but like ordinary
filesystem checks it cannot eliminate every time-of-check/time-of-use race
with another process.

`config` is read-only in this CLI. Edit the selected JSON file directly; there
is no atomic config-write command yet.

`models --offline --model MODEL` is the explicit no-network path. Other model
discovery uses the existing provider discovery service and Models.dev metadata;
model IDs are not hardcoded by the CLI.
