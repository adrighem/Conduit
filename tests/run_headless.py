#!/usr/bin/python3
"""Run a Python UI test in an isolated D-Bus session and Xvfb display."""

from __future__ import annotations

import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
from xml.sax.saxutils import escape


def wait_for_window_manager(environment: dict[str, str], process: subprocess.Popen) -> None:
    xprop = shutil.which("xprop")
    xdotool = shutil.which("xdotool")
    if xprop is None and xdotool is None:
        raise RuntimeError("xprop or xdotool is required to detect window-manager readiness")

    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("xfwm4 exited before becoming ready")
        if xprop is not None:
            supported = subprocess.run(
                [xprop, "-root", "_NET_SUPPORTED"],
                env=environment,
                capture_output=True,
                text=True,
                timeout=2,
            )
            if supported.returncode == 0 and "_NET_ACTIVE_WINDOW" in supported.stdout:
                return
        else:
            desktops = subprocess.run(
                [xdotool, "get_num_desktops"],
                env=environment,
                capture_output=True,
                text=True,
                timeout=2,
            )
            count = desktops.stdout.strip()
            if desktops.returncode == 0 and count.isdigit() and int(count) > 0:
                return
        time.sleep(0.1)
    raise RuntimeError("xfwm4 did not publish its EWMH state within 10 seconds")


def run_test() -> int:
    with tempfile.TemporaryDirectory(prefix="conduit-dbus-") as temporary:
        environment = os.environ.copy()
        # Headless/containerized test environments commonly disallow the user
        # namespaces required by WebKit's Bubblewrap sandbox. The activated
        # service inherits the D-Bus daemon's environment, so this must be set
        # before that daemon starts.
        environment["WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS"] = "1"
        config = Path(temporary) / "bus.conf"
        service_dir = os.environ.get("CONDUIT_TEST_DBUS_SERVICE_DIR")
        service_config = (
            f"  <servicedir>{escape(service_dir)}</servicedir>\n"
            if service_dir
            else ""
        )
        config.write_text(
            f"""<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
{service_config}  <policy context="default">
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
            env=environment,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.splitlines()
        environment["DBUS_SESSION_BUS_ADDRESS"] = output[0]
        window_manager = subprocess.Popen(
            ["xfwm4", "--replace"],
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            wait_for_window_manager(environment, window_manager)
            return subprocess.run(
                [sys.executable, *sys.argv[1:]], env=environment
            ).returncode
        finally:
            try:
                if window_manager.poll() is None:
                    window_manager.terminate()
                    try:
                        window_manager.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        window_manager.kill()
                        window_manager.wait(timeout=5)
            finally:
                try:
                    os.kill(int(output[1]), signal.SIGTERM)
                except ProcessLookupError:
                    pass


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
