#!/usr/bin/python3
"""Headless regression tests for Conduit's application keyboard shortcuts."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

SWITCHER_EVENT = "conversation-switcher-opened\n"


def wait_until(predicate, timeout: float = 40.0, interval: float = 0.1):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(interval)
    raise AssertionError(f"condition was not met within {timeout:.1f}s")


def compile_test_schema(schema: Path, directory: Path) -> None:
    shutil.copy2(schema, directory / schema.name)
    subprocess.run(
        ["glib-compile-schemas", "--strict", str(directory)],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def main() -> None:
    binary = Path(os.environ["CONDUIT_TEST_BINARY"])
    resource = Path(os.environ["CONDUIT_TEST_RESOURCE"])
    schema = Path(os.environ["CONDUIT_TEST_SCHEMA"])

    with tempfile.TemporaryDirectory(
        prefix="conduit-shortcuts-", ignore_cleanup_errors=True
    ) as temporary:
        temporary_path = Path(temporary)
        compile_test_schema(schema, temporary_path)
        event_log = temporary_path / "events.log"

        environment = os.environ.copy()
        environment.update(
            {
                "CONDUIT_RESOURCE_PATH": str(resource),
                "CONDUIT_TEST_WORKSPACE": "1",
                "CONDUIT_TEST_EVENT_LOG": str(event_log),
                "GSETTINGS_SCHEMA_DIR": str(temporary_path),
                "XDG_CACHE_HOME": str(temporary_path / "cache"),
                "XDG_CONFIG_HOME": str(temporary_path / "config"),
                "XDG_DATA_HOME": str(temporary_path / "data"),
            }
        )

        process = subprocess.Popen(
            [str(binary)],
            env=environment,
        )
        try:
            window_id = subprocess.run(
                [
                    "xdotool",
                    "search",
                    "--sync",
                    "--onlyvisible",
                    "--pid",
                    str(process.pid),
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=40,
            ).stdout.splitlines()[0]
            subprocess.run(
                ["xdotool", "windowactivate", "--sync", window_id], check=True
            )
            focused_window = subprocess.run(
                ["xdotool", "getwindowfocus"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            assert focused_window == window_id
            time.sleep(0.2)
            assert not event_log.exists()
            subprocess.run(
                [
                    "xdotool",
                    "keydown",
                    "Control_L",
                    "key",
                    "k",
                    "keyup",
                    "Control_L",
                ],
                check=True,
            )
            wait_until(
                lambda: event_log.exists() and event_log.read_text() == SWITCHER_EVENT,
                timeout=10.0,
            )
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)

        if process.returncode not in (0, -15):
            raise AssertionError(f"Conduit exited with {process.returncode}")


if __name__ == "__main__":
    main()
