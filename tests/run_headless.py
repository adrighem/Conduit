#!/usr/bin/python3
"""Run a Python UI test in an isolated D-Bus session and Xvfb display."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import tempfile


def run_test() -> int:
    with tempfile.TemporaryDirectory(prefix="conduit-dbus-") as temporary:
        config = Path(temporary) / "bus.conf"
        config.write_text(
            """<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"""
        )
        output = subprocess.run(
            [
                "dbus-daemon",
                f"--config-file={config}",
                "--fork",
                "--print-address=1",
                "--print-pid=1",
            ],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.splitlines()
        environment = os.environ.copy()
        environment["DBUS_SESSION_BUS_ADDRESS"] = output[0]
        window_manager = subprocess.Popen(
            ["xfwm4", "--replace"],
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            return subprocess.run(
                [sys.executable, *sys.argv[1:]], env=environment
            ).returncode
        finally:
            window_manager.terminate()
            window_manager.wait(timeout=5)
            os.kill(int(output[1]), 15)


if os.environ.get("CONDUIT_HEADLESS_INNER") == "1":
    raise SystemExit(run_test())

environment = os.environ.copy()
environment["CONDUIT_HEADLESS_INNER"] = "1"
os.execvpe(
    "xvfb-run",
    [
        "xvfb-run",
        "-a",
        sys.executable,
        __file__,
        *sys.argv[1:],
    ],
    environment,
)
