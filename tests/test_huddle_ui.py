#!/usr/bin/python3
"""Headless smoke test for the injected native huddle surface."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


APP_ID = "eu.vanadrighem.conduit"
APPLICATION_PATH = "/eu/vanadrighem/conduit"


def wait_until(predicate, timeout: float = 20.0, interval: float = 0.1):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(interval)
    raise AssertionError(f"condition was not met within {timeout:.1f}s")


def wait_for_window(process: subprocess.Popen[str]) -> str:
    def find_window() -> str | None:
        if process.poll() is not None:
            _, stderr = process.communicate()
            raise AssertionError(
                f"Conduit exited with {process.returncode} before showing a window:\n{stderr}"
            )
        result = subprocess.run(
            ["xdotool", "search", "--onlyvisible", "--pid", str(process.pid)],
            capture_output=True,
            text=True,
        )
        return next(iter(result.stdout.splitlines()), None)

    return wait_until(find_window)


def quit_application(environment: dict[str, str]) -> None:
    subprocess.run(
        [
            "gdbus",
            "call",
            "--session",
            "--dest",
            APP_ID,
            "--object-path",
            APPLICATION_PATH,
            "--method",
            "org.gtk.Actions.Activate",
            "quit",
            "[]",
            "{}",
        ],
        env=environment,
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    )


def main() -> None:
    binary = Path(os.environ["CONDUIT_TEST_BINARY"])
    resource = Path(os.environ["CONDUIT_TEST_RESOURCE"])
    schema = Path(os.environ["CONDUIT_TEST_SCHEMA"])

    with tempfile.TemporaryDirectory(
        prefix="conduit-huddle-ui-", ignore_cleanup_errors=True
    ) as temporary:
        root = Path(temporary)
        shutil.copy2(schema, root / schema.name)
        subprocess.run(
            ["glib-compile-schemas", "--strict", str(root)],
            check=True,
            capture_output=True,
            text=True,
        )
        state_path = root / "huddle-surface.json"
        external_uri_path = root / "huddle-external-uri"
        environment = os.environ.copy()
        environment.update(
            {
                "CONDUIT_RESOURCE_PATH": str(resource),
                "CONDUIT_TEST_WORKSPACE": "1",
                "CONDUIT_TEST_HUDDLE": "1",
                "CONDUIT_TEST_HUDDLE_UI_FILE": str(state_path),
                "CONDUIT_TEST_HUDDLE_EXTERNAL_URI_FILE": str(external_uri_path),
                "GSETTINGS_SCHEMA_DIR": str(root),
                "XDG_CACHE_HOME": str(root / "cache"),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_DATA_HOME": str(root / "data"),
            }
        )
        process = subprocess.Popen(
            [str(binary)],
            env=environment,
            text=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        try:
            wait_for_window(process)

            def visible_state() -> dict | None:
                if not state_path.exists():
                    return None
                state = json.loads(state_path.read_text(encoding="utf-8"))
                return state if state.get("visible") else None

            state = wait_until(visible_state)
            assert state == {
                "visible": True,
                "title": "Huddle is active",
                "primary_label": "View huddle",
                "camera_enabled": False,
                "screen_share_active": False,
            }
            external_uri = wait_until(
                lambda: external_uri_path.read_text(encoding="utf-8")
                if external_uri_path.exists()
                else None
            )
            assert external_uri == "https://app.slack.com/huddle/TTEST/CTEST"
            assert not external_uri.startswith("slack://")

            quit_application(environment)
            assert process.wait(timeout=10) == 0
            stderr = process.stderr.read() if process.stderr is not None else ""
            for marker in ("Gtk-ERROR", "Gtk-CRITICAL", "GLib-GObject-CRITICAL"):
                assert marker not in stderr, stderr
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


if __name__ == "__main__":
    main()
