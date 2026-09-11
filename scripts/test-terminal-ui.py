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

        def launch(theme, store, first=False, no_color=False):
            exit_status.unlink(missing_ok=True)
            command = shlex.join([
                str(binary), "ui", "--theme", theme,
                "--store", str(store), "--config", str(config),
            ])
            environment = "export NO_COLOR=1; " if no_color else "unset NO_COLOR; "
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
            expect("Your first verified movement.")

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
            expect("Your first verified movement.")
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
