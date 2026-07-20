#!/usr/bin/python3
"""Verify that main-window size and maximized state survive a restart."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


APP_ID = "eu.vanadrighem.conduit"
APPLICATION_PATH = "/eu/vanadrighem/conduit"
EXPECTED_SIZE = (920, 640)
SIZE_TOLERANCE = 4


def wait_until(predicate, timeout: float = 15.0, interval: float = 0.1):
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


def window_size(window_id: str) -> tuple[int, int]:
    result = subprocess.run(
        ["xdotool", "getwindowgeometry", "--shell", window_id],
        check=True,
        capture_output=True,
        text=True,
    )
    values = dict(
        line.split("=", 1) for line in result.stdout.splitlines() if "=" in line
    )
    return int(values["WIDTH"]), int(values["HEIGHT"])


def wait_for_size(
    window_id: str,
    expected: tuple[int, int],
    timeout: float = 15.0,
    stable_for: float = 0.75,
) -> None:
    deadline = time.monotonic() + timeout
    stable_since: float | None = None
    actual = window_size(window_id)
    while time.monotonic() < deadline:
        actual = window_size(window_id)
        if all(
            abs(left - right) <= SIZE_TOLERANCE
            for left, right in zip(actual, expected)
        ):
            stable_since = stable_since or time.monotonic()
            if time.monotonic() - stable_since >= stable_for:
                return
        else:
            stable_since = None
        time.sleep(0.1)
    raise AssertionError(f"window size remained {actual}, expected {expected}")


def resize_window(window_id: str, expected: tuple[int, int]) -> None:
    subprocess.run(
        ["xdotool", "windowactivate", "--sync", window_id], check=True
    )
    subprocess.run(
        ["xdotool", "windowsize", "--sync", window_id, *map(str, expected)],
        check=True,
    )
    wait_for_size(window_id, expected)


def window_is_maximized(window_id: str) -> bool:
    result = subprocess.run(
        ["xprop", "-id", window_id, "_NET_WM_STATE"],
        capture_output=True,
        text=True,
    )
    return all(
        state in result.stdout
        for state in ("_NET_WM_STATE_MAXIMIZED_HORZ", "_NET_WM_STATE_MAXIMIZED_VERT")
    )


def toggle_maximized(window_id: str) -> None:
    width, _ = window_size(window_id)
    subprocess.run(
        [
            "xdotool",
            "mousemove",
            "--window",
            window_id,
            str(width // 2),
            "16",
            "click",
            "--repeat",
            "2",
            "--delay",
            "100",
            "1",
        ],
        check=True,
    )


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


def run_application(binary: Path, environment: dict[str, str]):
    process = subprocess.Popen(
        [str(binary)],
        env=environment,
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    return process, wait_for_window(process)


def stop_application(process: subprocess.Popen[str], environment: dict[str, str]) -> None:
    quit_application(environment)
    assert process.wait(timeout=10) == 0
    stderr = process.stderr.read() if process.stderr is not None else ""
    for marker in ("Gtk-ERROR", "Gtk-CRITICAL", "GLib-GObject-CRITICAL"):
        assert marker not in stderr, stderr


def terminate_if_running(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def main() -> None:
    binary = Path(os.environ["CONDUIT_TEST_BINARY"])
    resource = Path(os.environ["CONDUIT_TEST_RESOURCE"])
    schema = Path(os.environ["CONDUIT_TEST_SCHEMA"])

    with tempfile.TemporaryDirectory(
        prefix="conduit-window-state-", ignore_cleanup_errors=True
    ) as temporary:
        root = Path(temporary)
        shutil.copy2(schema, root / schema.name)
        subprocess.run(
            ["glib-compile-schemas", "--strict", str(root)],
            check=True,
            capture_output=True,
            text=True,
        )
        environment = os.environ.copy()
        environment.update(
            {
                "CONDUIT_RESOURCE_PATH": str(resource),
                "CONDUIT_TEST_WORKSPACE": "1",
                "GSETTINGS_BACKEND": "keyfile",
                "GSETTINGS_SCHEMA_DIR": str(root),
                "XDG_CACHE_HOME": str(root / "cache"),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_DATA_HOME": str(root / "data"),
            }
        )

        process: subprocess.Popen[str] | None = None
        try:
            process, window_id = run_application(binary, environment)
            resize_window(window_id, EXPECTED_SIZE)
            stop_application(process, environment)
            process = None

            process, window_id = run_application(binary, environment)
            wait_for_size(window_id, EXPECTED_SIZE)
            subprocess.run(
                ["xdotool", "windowactivate", "--sync", window_id], check=True
            )
            toggle_maximized(window_id)
            wait_until(lambda: window_is_maximized(window_id))
            stop_application(process, environment)
            process = None

            process, window_id = run_application(binary, environment)
            wait_until(lambda: window_is_maximized(window_id))
            stop_application(process, environment)
            process = None

            subprocess.run(
                ["gsettings", "set", APP_ID, "window-maximized", "false"],
                env=environment,
                check=True,
            )
            process, window_id = run_application(binary, environment)
            wait_for_size(window_id, EXPECTED_SIZE)
            stop_application(process, environment)
            process = None
        finally:
            terminate_if_running(process)


if __name__ == "__main__":
    main()
