#!/usr/bin/env python3
"""Exercise the actual CLI in an isolated tmux PTY (Linux/macOS, tmux required)."""

import argparse
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time
import uuid
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--artifacts", type=Path)
    args = parser.parse_args()
    if not shutil.which("tmux"):
        parser.error("tmux is required; this check has NOT run")
    binary = args.binary.resolve(strict=True)
    if args.artifacts:
        args.artifacts.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="servoloop-ui-") as directory:
        root = Path(directory)
        config = root / "config.json"
        config.write_text('{"version":1}')
        tmux_config = root / "tmux.conf"
        tmux_config.write_text(
            'set -g default-terminal "xterm-256color"\n'
            "set -g status off\n"
            "set -g remain-on-exit on\n"
        )
        server = "servoloop-test-" + uuid.uuid4().hex
        exit_status = root / "exit-status"

        def tmux(*arguments):
            return subprocess.check_output(
                ["tmux", "-L", server, "-f", str(tmux_config), *arguments],
                text=True,
                stderr=subprocess.STDOUT,
                timeout=10,
            )

        def capture():
            return tmux("capture-pane", "-p", "-t", "ui:0.0")

        def expect(text):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                screen = capture()
                if text in screen:
                    return screen
                time.sleep(0.025)
            raise AssertionError(f"Did not render {text!r}:\n{capture()}")

        def key(*keys):
            tmux("send-keys", "-t", "ui:0.0", *keys)

        def artifact(name):
            if args.artifacts:
                (args.artifacts / f"{name}.txt").write_text(capture())
                (args.artifacts / f"{name}.ansi").write_text(
                    tmux("capture-pane", "-p", "-e", "-t", "ui:0.0")
                )

        def launch(theme, store, first=False, no_color=False, catalog_url="http://127.0.0.1:9/catalog.json"):
            exit_status.unlink(missing_ok=True)
            command = shlex.join([
                str(binary), "ui", "--theme", theme,
                "--store", str(store), "--config", str(config),
            ])
            environment = "unset SERVOLOOP_PROVIDER SERVOLOOP_MODEL SERVOLOOP_BASE_URL; "
            environment += "export SERVOLOOP_MODELS_DEV_URL=" + shlex.quote(catalog_url) + "; "
            environment += "export CATALOG_FIXTURE_API_KEY=catalog-fixture-secret-value; "
            environment += "export NO_COLOR=1; " if no_color else "unset NO_COLOR; "
            shell = (
                environment + "before=$(stty -g); " + command + "; result=$?; "
                'after=$(stty -g); if [ "$before" = "$after" ]; then restored=yes; '
                'else restored=no; fi; printf "%s %s" "$result" "$restored" > '
                + shlex.quote(str(exit_status))
            )
            if first:
                tmux("new-session", "-d", "-s", "ui", "-x", "120", "-y", "34", shell)
            else:
                tmux("respawn-pane", "-k", "-t", "ui:0.0", shell)
            expect("Start with a simulated arm.")

        def expect_exit():
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if exit_status.exists() and exit_status.read_text():
                    assert exit_status.read_text() == "0 yes", "Exit failed or terminal mode was not restored"
                    return
                time.sleep(0.025)
            raise AssertionError("UI did not exit and restore terminal settings")

        try:
            store = root / "store"
            launch("dark", store, first=True)
            assert not store.exists(), "Opening the UI wrote to the store"
            artifact("welcome-dark")
            key("Enter")
            expect("Review the offline demo")
            key("Enter")  # Reviewing twice must not execute.
            # Pasted keys must be handled as paste, never as confirmation.
            tmux("send-keys", "-t", "ui:0.0", "-l", "\x1b[200~r\r\x1b[201~")
            key("?")
            expect("A terminal workspace, not a second runtime")
            key("Escape")
            expect("Review the offline demo")
            assert not store.exists(), "Preview, paste, or help started execution"
            tmux("resize-window", "-t", "ui:0", "-x", "40", "-y", "12")
            expect("Enlarge to at least")
            key("r")
            # Check the invariant over several render ticks before resizing;
            # otherwise the resize can overtake the queued input in the PTY.
            deadline = time.monotonic() + 0.2
            while time.monotonic() < deadline:
                assert not store.exists(), "Hidden preview accepted a motion shortcut"
                time.sleep(0.025)
            tmux("resize-window", "-t", "ui:0", "-x", "120", "-y", "34")
            expect("Review the offline demo")
            assert not store.exists(), "Hidden preview accepted a motion shortcut"
            artifact("review-dark")
            key("r")
            expect("Complete / snapshot saved")
            screen = expect("Position verified: 0.200 rad")
            assert "Snapshot saved" in screen
            artifact("result-dark")
            sessions = list(store.iterdir())
            assert len(sessions) == 1
            records = [json.loads(line) for line in (sessions[0] / "journal.ndjson").read_text().splitlines()]
            assert len(records) == 2
            assert records[0]["kind"] == "intent"
            assert records[1]["outcome"] == "verified"
            assert (sessions[0] / "snapshot.json").exists()
            key("i")
            expect("Session / durable record")
            artifact("inspect-dark")
            key("Escape")
            expect("Offline shoulder check")
            tmux("resize-window", "-t", "ui:0", "-x", "60", "-y", "24")
            expect("n new demo")
            artifact("result-narrow")
            key("n")
            expect("Review the offline demo")
            key("Escape")
            expect("Start with a simulated arm.")
            assert len(list(store.iterdir())) == 1, "Navigation replayed motion"
            tmux("resize-window", "-t", "ui:0", "-x", "40", "-y", "12")
            expect("Enlarge to at least")
            key("q")
            expect_exit()

            tmux("resize-window", "-t", "ui:0", "-x", "80", "-y", "24")
            for theme in ["light", "mono"]:
                launch(theme, root / theme)
                artifact(f"welcome-{theme}")
                key("q")
                expect_exit()
                assert not (root / theme).exists()

            launch("dark", root / "no-color", no_color=True)
            ansi = tmux("capture-pane", "-p", "-e", "-t", "ui:0.0")
            assert "38;2;" not in ansi and "48;2;" not in ansi
            key("C-c")
            expect_exit()
            assert not (root / "no-color").exists()

            # Real setup, discovery, send and resume against a loopback fixture.
            # No provider credentials or external service are used.
            requests = []
            class ProviderFixture(BaseHTTPRequestHandler):
                catalog_error = True
                extra_provider = False
                catalog_requests = 0
                expected_key = "workspace-fixture-key"
                def log_message(self, *unused):
                    pass

                def reply(self, value):
                    body = json.dumps(value).encode()
                    if self.path == "/catalog.json":
                        # Match the real catalog size that exceeded the old
                        # shared 4 MiB cap; exercise this through the actual UI.
                        body += b" " * max(0, 4_614_121 - len(body))
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

                def do_GET(self):
                    if self.path == "/catalog.json":
                        assert self.headers.get("Authorization") is None, "Catalog request included provider credentials"
                        type(self).catalog_requests += 1
                        if type(self).catalog_error:
                            self.send_error(503, "Catalog fixture unavailable")
                            return
                        model_data = {
                            "fixture-model": {"name": "Fixture", "tool_call": True, "limit": {"context": 8192}},
                            "catalog-only-model": {"name": "Catalog only", "tool_call": False}
                        }
                        catalog = {
                            "ollama": {"name": "Ollama", "npm": "@ai-sdk/openai-compatible", "api": f"http://127.0.0.1:{self.server.server_port}/v1", "env": [], "models": model_data},
                            "local-catalog-lab": {"name": "Local Catalog Lab", "npm": "@ai-sdk/openai-compatible", "api": f"http://127.0.0.1:{self.server.server_port}/v1", "env": ["CATALOG_FIXTURE_API_KEY"], "models": model_data},
                            "unsupported-fixture": {"name": "Unsupported Fixture", "npm": "@ai-sdk/anthropic", "api": "https://unused.invalid", "env": [], "models": model_data}
                        }
                        if type(self).extra_provider:
                            catalog["refresh-added-provider"] = {"name": "Refresh Added Provider", "npm": "@ai-sdk/openai-compatible",
                                "api": f"http://127.0.0.1:{self.server.server_port}/v1", "env": [], "models": model_data}
                        self.reply(catalog)
                    else:
                        self.reply({"data": [{"id": "fixture-model"}], "models": [{"name": "fixture-model"}]})

                def do_POST(self):
                    request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    assert self.headers.get("Authorization") == f"Bearer {type(self).expected_key}"
                    requests.append(request)
                    self.reply({"choices": [{"message": {"content": f"Local fixture response. No robot tools requested. {type(self).expected_key}"}, "finish_reason": "stop"}]})

            with ThreadingHTTPServer(("127.0.0.1", 0), ProviderFixture) as provider:
                thread = threading.Thread(target=provider.serve_forever, daemon=True)
                thread.start()
                try:
                    config.write_text(json.dumps({"version": 1, "provider": "ollama", "model": "fixture-model",
                        "base_url": f"http://127.0.0.1:{provider.server_port}/v1"}))
                    tmux("resize-window", "-t", "ui:0", "-x", "120", "-y", "34")
                    live_store = root / "provider-store"
                    launch("dark", live_store, catalog_url=f"http://127.0.0.1:{provider.server_port}/catalog.json")
                    key("Down", "Enter")
                    expect("Pending changes apply to the next run")
                    artifact("provider-setup")
                    key("Enter")
                    expect("Choose a provider")
                    expect("Provider catalog unavailable")
                    expect("0 matches")
                    assert not live_store.exists()
                    ProviderFixture.catalog_error = False
                    key("F5")
                    expect("3 matches")
                    expect("unsupported adapter")
                    ProviderFixture.extra_provider = True
                    key("F5")
                    expect("4 matches")
                    expect("Refresh Added Provider")
                    catalog_reads = ProviderFixture.catalog_requests
                    artifact("provider-picker-all")
                    key("q")
                    expect("No matching provider")
                    key("BSpace", "ollama")
                    expect("1 matches")
                    artifact("provider-picker")
                    key("Escape")
                    expect("Selection cancelled")
                    key("Enter", "local-catalog-lab")
                    expect("1 matches")
                    assert ProviderFixture.catalog_requests == catalog_reads, "Provider popup ignored the catalog cache"
                    key("Enter")
                    expect("Provider selected")
                    expect("CATALOG_FIXTURE_API_KEY")
                    key("Tab", "Tab", "Enter")
                    expect("Add API key")
                    key("workspace-fixture-key")
                    expect("•" * len("workspace-fixture-key"))
                    assert "workspace-fixture-key" not in capture(), "API key input was not masked"
                    artifact("api-key-entry")
                    key("Enter")
                    expect("Workspace API key")
                    key("Tab", "Tab", "Enter")
                    expect("Endpoint returned 1 models")
                    expect("Choose a model")
                    expect("catalog-only-model")
                    artifact("model-picker-all")
                    key("fixture-model")
                    expect("1 matches")
                    expect("Tools: supported")
                    artifact("provider-models")
                    assert requests == [], "Discovery dispatched a model request"
                    key("Enter")
                    expect("Model selected")
                    key("Tab", "Enter")
                    expect("What would you like to test?")
                    assert json.loads(config.read_text())["model"] == "fixture-model"
                    assert json.loads(config.read_text())["provider"] == "local-catalog-lab"
                    assert json.loads(config.read_text())["provider_profile"]["credential_env"] == ["CATALOG_FIXTURE_API_KEY"]
                    assert "catalog-fixture-secret-value" not in config.read_text()
                    assert "workspace-fixture-key" not in config.read_text()
                    key("1")
                    expect("Inspect the current joint positions")
                    key("/")
                    expect("Search commands")
                    artifact("command-palette")
                    key("Escape")
                    expect("What would you like to test?")
                    assert requests == [], "Navigation or suggestion submitted a request"
                    artifact("conversation-draft")
                    key("s")
                    expect("Complete / snapshot saved")
                    expect("Local fixture response")
                    assert len(requests) == 1
                    assert "Position verified:" not in capture(), "Model prose became physical verification"
                    assert "catalog-fixture-secret-value" not in capture(), "Catalog credential leaked into terminal output"
                    snapshot = next(live_store.glob("*/snapshot.json"))
                    assert "catalog-fixture-secret-value" not in snapshot.read_text(), "Catalog credential leaked into the saved snapshot"
                    assert "workspace-fixture-key" not in capture(), "Entered credential leaked into terminal output"
                    assert "workspace-fixture-key" not in snapshot.read_text(), "Entered credential leaked into snapshot"
                    artifact("provider-result")
                    key("/")
                    expect("Search commands")
                    key("Down", "Enter")
                    expect("1 saved sessions")
                    artifact("session-browser")
                    key("Enter")
                    expect("Session and journal consistency checks passed")
                    artifact("resume-preflight")
                    assert len(requests) == 1, "Preflight contacted the model"
                    key("Enter")
                    expect("Resumed conversation")
                    artifact("resumed-conversation")
                    key("3", "s")
                    expect("Complete / snapshot saved")
                    assert len(requests) == 2
                    assert len(list(live_store.iterdir())) == 1, "Continuation created a duplicate session"
                    assert "Local fixture response" in json.dumps(requests[1]), "Saved history was not sent on resume"
                    key("q")
                    expect_exit()
                    # Reload the saved, dynamically sourced connection in a
                    # fresh process and resume without selecting it again.
                    ProviderFixture.expected_key = "catalog-fixture-secret-value"
                    launch("dark", live_store, catalog_url=f"http://127.0.0.1:{provider.server_port}/catalog.json")
                    key("Down", "Down", "Enter")
                    expect("1 saved sessions")
                    key("Enter")
                    expect("Session and journal consistency checks passed")
                    key("Enter", "3", "s")
                    expect("Complete / snapshot saved")
                    assert len(requests) == 3
                    assert "catalog-fixture-secret-value" not in snapshot.read_text()
                    key("q")
                    expect_exit()
                finally:
                    provider.shutdown()
                    thread.join(timeout=5)

            bad_store = root / "not-a-directory"
            bad_store.write_text("preserve this file")
            launch("dark", bad_store)
            key("Enter")
            expect("Review the offline demo")
            key("r")
            screen = expect("Failed / inspect before continuing")
            assert "Position verified:" not in screen
            artifact("storage-failure")
            key("n")
            key("i")
            expect("Session / durable record")
            assert "not opened" in capture()
            assert bad_store.read_text() == "preserve this file"
            key("q")
            expect_exit()
            print("PASS: welcome, preview, paste, real demo, persistence, inspect, resize, themes, failure, no replay, terminal restoration")
        finally:
            subprocess.run(["tmux", "-L", server, "kill-server"], capture_output=True, timeout=10)


if __name__ == "__main__":
    main()
