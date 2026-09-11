# ServoLoop CLI

`servoloop` emits NDJSON events on standard output for both `run --demo` and
live `run --prompt TEXT`, and `resume SESSION --prompt TEXT` executions;
diagnostics always go to standard error.

## Interactive terminal workspace

Open the simulation-only workspace in a normal terminal:

```sh
cargo run --locked -p servoloop-cli -- ui
```

Press **Enter** to review the offline demo, then **r** to authorize its fixed
shoulder movement. Opening the UI, visiting help, and reviewing the demo do not
create sessions or dispatch commands. The demo uses the existing agent loop,
simulated driver, and durable journal; it never contacts a configured provider.

Use **i** to inspect results, **Esc** to return, and **q** to quit. During a run,
**Ctrl+C** or **q** requests cancellation and waits for cleanup; press **q**
again after it settles to exit. Cancellation cannot reverse motion. Failed or
interrupted runs offer inspection, not a motion-retry action.

Choose `--theme dark`, `--theme light`, or `--theme mono`. `NO_COLOR` selects
monochrome regardless of that flag. Use `--store DIRECTORY` for a separate
session store. At least 48 columns and 18 rows are required; smaller windows
disable hidden actions but retain cancellation and exit. Piped input/output
and `TERM=dumb` are rejected without writing session data. Use the existing
line-oriented commands for automation and screen-reader workflows.

This first slice supports the fixed offline demo, not arbitrary prompts,
provider setup, saved-session browsing, or updates inside the UI. See the
[design audit and implementation scope](../../docs/terminal-workspace.md).

## Implementation and persistence

Interactive and scriptable commands share the same execution path.

The CLI keeps composition in `src/main.rs` and groups responsibilities by
boundary: typed argument compatibility parsing is in `args.rs`, configuration
and private directory resolution in `config.rs`, redacted output in
`output.rs`, execution and lifecycle ownership in `execution.rs`, and the
simulator and durable tool decorator in `simulation.rs` and `journal_tool.rs`.
Command-specific discovery and session operations live in `commands.rs`.
`ui/` separates keyboard navigation and terminal lifecycle, bounded presentation
state, and layout. `RunOutput` keeps NDJSON and terminal presentation separate.
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

`config show` and `config validate` are read-only. `config init` creates a
starter file and refuses an existing destination. Edit configuration explicitly;
the terminal workspace does not write provider settings.

`models --offline --model MODEL` is the explicit no-network path. Other model
discovery uses the existing provider discovery service and Models.dev metadata;
model IDs are not hardcoded by the CLI.

## Terminal verification

Unit tests exercise navigation, evidence semantics, bounded/redacted output,
themes, rendering, and the shared execution path. On Linux with `tmux` installed,
you can also exercise the real terminal program:

```sh
cargo build --locked -p servoloop-cli
python3 scripts/test-terminal-ui.py target/debug/servoloop
```

The smoke test uses an isolated tmux server and temporary session store. It
checks preview and paste safety, real journal/snapshot writes, inspector
navigation, resizing, themes, storage failure, no navigation-triggered replay,
and terminal-mode restoration. It never attaches to your tmux sessions.
