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

The welcome screen also opens **Configure a provider** and **Open a saved
session**. Press **/** outside an editor to open the command palette. See the
[design audit and implementation scope](../../docs/terminal-workspace.md).

### Provider and model pickers

In provider setup, use **Tab** to focus a field and **Enter** to open it. Provider
and model fields open centered, searchable popups. Type to filter, use **Up/Down**
to navigate, press **Enter** to select, or **Esc** to cancel without changing the
pending selection. **F5** refreshes either catalog picker. **m** in a conversation opens
the model picker without discarding your draft.

Providers come from Models.dev's `api.json`, not a fixed built-in list. Their
names, endpoints, credential references, and adapter metadata come from the
catalog. Providers with unsupported or unknown adapters remain visible but
cannot be selected for execution. OpenAI-compatible catalog connections are
stored with the selected configuration and work without adding provider IDs to
the CLI source. Existing built-in CLI configurations remain compatible.

Models come from the existing discovery service: the same Models.dev catalog,
the provider endpoint, and Ollama's native tags endpoint. Set
`SERVOLOOP_MODELS_DEV_URL` to an HTTP(S) catalog URL for a compatible mirror or
local fixture. Opening model selection can contact these services; the offline
demo never does. Provider entries are cached for five minutes; model results
use the discovery service's in-memory cache. Refresh failure keeps any cached
entries and shows the error; an initial failure never substitutes a fixed list.
Catalog responses have a 16 MiB download cap, enforced incrementally. Ordinary
provider JSON responses retain their separate 4 MiB cap.

Each model shows reported tool/image capabilities, context size, and whether
the endpoint listed it. Catalog-only entries are not proof of account access,
and unknown capabilities stay unknown. If discovery fails, type an exact model
ID and select **Use custom model**. Selecting does not send a prompt or save
settings. **Save and continue** explicitly commits the configuration, including
the catalog connection metadata, and applies it to the next run.

To enter an API key, select **Authentication / Enter API key** in provider setup.
Type or paste the key into the masked field, then press **Enter** to use it or
**Esc** to cancel. Entered keys override environment credentials for that
provider and endpoint in the current workspace only. **Ctrl+R** in the key dialog
forgets the workspace key and restores environment fallback. Changing the
endpoint does not forward the entered key to the new destination.

Keys stay in memory and are not saved by **Save and continue**. After restarting,
enter the key again or configure the referenced environment variable. Credential
values are redacted from output and snapshots and never included in saved
settings. Catalog lookup sends no provider credentials. Browser/OAuth login is
not implemented; providers requiring it still need a dedicated integration.
Unsupported credential references do not cause arbitrary environment reads.

### Conversations and saved sessions

In a conversation, **1**, **2**, or **3** fills a suggested draft without sending
it. Press **e** to edit, **Enter** to finish editing, and **s** to send through the
existing provider-backed execution loop. The current editor supports bounded,
single-line prompts. Press **c** after successful execution to continue the
conversation. Model-response previews are bounded; inspect the saved snapshot
for the complete stored conversation.

The session browser supports **e** to search, **Up/Down** to choose, and **Enter**
to review. Preflight checks the snapshot, journal, and execution lease before
offering continuation. Send repeats those checks while holding the lease through
execution. Historical messages are not current robot state, and no earlier
commands are replayed. Locked, corrupt, or unresolved sessions remain blocked.

The update view currently provides installation guidance only. It does not
check a registry, assume a package manager, or install anything. The complete
Paper recovery layouts and verified installer workflow remain follow-up work.

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
starter file and refuses an existing destination. The terminal workspace's
explicit **Save and continue** atomically replaces the selected config file,
preserves unrelated JSON fields, and refuses stale edits, symlinks, or an existing
config-write lock. It does not modify an in-flight run. As with store operations,
use a trusted parent directory; external editors do not participate in its lock.

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
and terminal-mode restoration. A loopback HTTP fixture also verifies provider
selection, catalog/endpoint model discovery, config saving, prompt submission,
session browsing, and resume with real saved history. It never attaches to your
tmux sessions or uses live provider credentials.
