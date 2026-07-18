#!/usr/bin/python3
"""Headless smoke test for command-line and D-Bus Slack URI activation."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


APP_ID = "eu.vanadrighem.conduit"
APPLICATION_PATH = "/eu/vanadrighem/conduit"


def wait_until(predicate, timeout: float = 10.0, interval: float = 0.1):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(interval)
    raise AssertionError(f"condition was not met within {timeout:.1f}s")


def bus_name_owned(environment: dict[str, str]) -> bool:
    output = subprocess.run(
        [
            "gdbus",
            "call",
            "--session",
            "--dest",
            "org.freedesktop.DBus",
            "--object-path",
            "/org/freedesktop/DBus",
            "--method",
            "org.freedesktop.DBus.NameHasOwner",
            APP_ID,
        ],
        env=environment,
        check=True,
        capture_output=True,
        text=True,
        timeout=5,
    ).stdout
    return "true" in output.lower()


def open_slack_uri(environment: dict[str, str]) -> None:
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
            "org.freedesktop.Application.Open",
            "['slack://open']",
            "{}",
        ],
        env=environment,
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
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


def marker_count(path: Path) -> int:
    if not path.exists():
        return 0
    return len(path.read_text(encoding="utf-8").splitlines())


def main() -> None:
    binary = Path(os.environ["CONDUIT_TEST_BINARY"])
    resource = Path(os.environ["CONDUIT_TEST_RESOURCE"])
    schema = Path(os.environ["CONDUIT_TEST_SCHEMA"])

    with tempfile.TemporaryDirectory(
        prefix="conduit-slack-uri-", ignore_cleanup_errors=True
    ) as temporary:
        root = Path(temporary)
        shutil.copy2(schema, root / schema.name)
        subprocess.run(
            ["glib-compile-schemas", "--strict", str(root)],
            check=True,
            capture_output=True,
            text=True,
        )
        marker = root / "opened-slack-uris"
        log_path = root / "conduit.log"
        environment = os.environ.copy()
        environment.update(
            {
                "CONDUIT_RESOURCE_PATH": str(resource),
                "CONDUIT_TEST_WORKSPACE": "1",
                "CONDUIT_TEST_OPEN_SLACK_URI_FILE": str(marker),
                "GSETTINGS_SCHEMA_DIR": str(root),
                "XDG_CACHE_HOME": str(root / "cache"),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_DATA_HOME": str(root / "data"),
            }
        )

        with log_path.open("w", encoding="utf-8") as log:
            process = subprocess.Popen(
                [binary, "slack://open"],
                env=environment,
                stdout=log,
                stderr=subprocess.STDOUT,
                text=True,
            )
            try:
                wait_until(lambda: bus_name_owned(environment))
                wait_until(lambda: marker_count(marker) == 1)

                open_slack_uri(environment)
                wait_until(lambda: marker_count(marker) == 2)

                quit_application(environment)
                assert process.wait(timeout=10) == 0
                wait_until(lambda: not bus_name_owned(environment))
            except Exception:
                log.flush()
                print(log_path.read_text(encoding="utf-8"))
                raise
            finally:
                if process.poll() is None:
                    if bus_name_owned(environment):
                        try:
                            quit_application(environment)
                            process.wait(timeout=5)
                        except (subprocess.SubprocessError, OSError):
                            pass
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=5)


if __name__ == "__main__":
    main()
