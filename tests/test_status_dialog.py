#!/usr/bin/python3
"""Headless smoke test for the current-user header and native status dialog."""

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
        prefix="conduit-status-ui-", ignore_cleanup_errors=True
    ) as temporary:
        root = Path(temporary)
        shutil.copy2(schema, root / schema.name)
        subprocess.run(
            ["glib-compile-schemas", "--strict", str(root)],
            check=True,
            capture_output=True,
            text=True,
        )
        base_environment = os.environ.copy()
        base_environment.update(
            {
                "CONDUIT_RESOURCE_PATH": str(resource),
                "CONDUIT_TEST_WORKSPACE": "1",
                "CONDUIT_TEST_STATUS_DIALOG": "1",
                "GSETTINGS_SCHEMA_DIR": str(root),
                "XDG_CACHE_HOME": str(root / "cache"),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_DATA_HOME": str(root / "data"),
            }
        )
        cases = [
            {
                "name": "empty-wide",
                "extra_environment": {},
                "save_enabled": False,
                "clear_available": False,
                "status_has_value": False,
                "header_subtitle": "",
                "maximum_width": None,
            },
            {
                "name": "preset-narrow",
                "extra_environment": {
                    "CONDUIT_TEST_STATUS_NARROW": "1",
                    "CONDUIT_TEST_STATUS_PRESET": "1",
                },
                "save_enabled": True,
                "clear_available": True,
                "status_has_value": True,
                "header_subtitle": "🏠 Working remotely",
                "maximum_width": 400,
            },
        ]

        for case in cases:
            state_path = root / f"{case['name']}.json"
            environment = base_environment.copy()
            environment.update(case["extra_environment"])
            environment["CONDUIT_TEST_STATUS_UI_FILE"] = str(state_path)
            process = subprocess.Popen(
                [str(binary)],
                env=environment,
                text=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            try:
                wait_for_window(process)

                def expected_state() -> dict | None:
                    if not state_path.exists():
                        return None
                    try:
                        state = json.loads(state_path.read_text(encoding="utf-8"))
                    except json.JSONDecodeError:
                        return None
                    width_matches = case["maximum_width"] is None or (
                        0 < state.get("window_width", 0) <= case["maximum_width"]
                    )
                    if (
                        state.get("dialog_heading") == "Set a status"
                        and state.get("emoji_search") is True
                        and state.get("emoji_choice_count", 0) > 100
                        and state.get("expiration_choice_count") == 6
                        and state.get("save_enabled") == case["save_enabled"]
                        and state.get("clear_available") == case["clear_available"]
                        and state.get("status_has_value") == case["status_has_value"]
                        and state.get("header_title") == "Test User"
                        and state.get("header_subtitle") == case["header_subtitle"]
                        and width_matches
                    ):
                        return state
                    return None

                wait_until(expected_state)

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
