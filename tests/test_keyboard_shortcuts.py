#!/usr/bin/python3
"""Headless regression tests for Conduit's application keyboard shortcuts."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

SWITCHER_TITLE = "Switch conversation"


def wait_until(predicate, timeout: float = 40.0, interval: float = 0.1):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(interval)
    raise AssertionError(f"condition was not met within {timeout:.1f}s")


def wait_for_window(process: subprocess.Popen[str], timeout: float = 40.0) -> str:
    def find_window() -> str | None:
        return_code = process.poll()
        if return_code is not None:
            _, stderr = process.communicate()
            raise AssertionError(
                f"Conduit exited with {return_code} before showing a window:\n{stderr}"
            )
        result = subprocess.run(
            [
                "xdotool",
                "search",
                "--onlyvisible",
                "--pid",
                str(process.pid),
            ],
            capture_output=True,
            text=True,
        )
        return next(iter(result.stdout.splitlines()), None)

    return wait_until(find_window, timeout=timeout)


def compile_test_schema(schema: Path, directory: Path) -> None:
    shutil.copy2(schema, directory / schema.name)
    subprocess.run(
        ["glib-compile-schemas", "--strict", str(directory)],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def visible_window_ids(name: str) -> list[str]:
    result = subprocess.run(
        ["xdotool", "search", "--onlyvisible", "--name", f"^{name}$"],
        capture_output=True,
        text=True,
    )
    return result.stdout.splitlines() if result.returncode == 0 else []


def main() -> None:
    binary = Path(os.environ["CONDUIT_TEST_BINARY"])
    resource = Path(os.environ["CONDUIT_TEST_RESOURCE"])
    schema = Path(os.environ["CONDUIT_TEST_SCHEMA"])

    with tempfile.TemporaryDirectory(
        prefix="conduit-shortcuts-", ignore_cleanup_errors=True
    ) as temporary:
        temporary_path = Path(temporary)
        compile_test_schema(schema, temporary_path)
        environment = os.environ.copy()
        environment.update(
            {
                "CONDUIT_RESOURCE_PATH": str(resource),
                "CONDUIT_TEST_WORKSPACE": "1",
                "GSETTINGS_SCHEMA_DIR": str(temporary_path),
                "XDG_CACHE_HOME": str(temporary_path / "cache"),
                "XDG_CONFIG_HOME": str(temporary_path / "config"),
                "XDG_DATA_HOME": str(temporary_path / "data"),
            }
        )

        process = subprocess.Popen(
            [str(binary)],
            env=environment,
            text=True,
            stderr=subprocess.PIPE,
        )
        try:
            window_id = wait_for_window(process)
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
            assert not visible_window_ids(SWITCHER_TITLE)
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
            switcher_id = wait_until(lambda: next(iter(visible_window_ids(SWITCHER_TITLE)), None))
            subprocess.run(
                ["xdotool", "windowactivate", "--sync", switcher_id], check=True
            )
            subprocess.run(["xdotool", "key", "Escape"], check=True)
            wait_until(lambda: not visible_window_ids(SWITCHER_TITLE), timeout=10.0)
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
