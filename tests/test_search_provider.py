#!/usr/bin/python3
"""Headless D-Bus smoke test for the GNOME Shell search provider."""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time

APP_ID = "eu.vanadrighem.conduit"
OBJECT_PATH = "/eu/vanadrighem/conduit/SearchProvider"
INTERFACE = "org.gnome.Shell.SearchProvider2"


def wait_until(predicate, timeout: float = 30.0, interval: float = 0.1):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(interval)
    raise AssertionError(f"condition was not met within {timeout:.1f}s")


def call(environment: dict[str, str], method: str, *arguments: str) -> str:
    return subprocess.run(
        [
            "gdbus",
            "call",
            "--session",
            "--dest",
            APP_ID,
            "--object-path",
            OBJECT_PATH,
            "--method",
            f"{INTERFACE}.{method}",
            *arguments,
        ],
        env=environment,
        check=True,
        capture_output=True,
        text=True,
        timeout=10,
    ).stdout


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


def quit_application(environment: dict[str, str]) -> None:
    subprocess.run(
        [
            "gdbus",
            "call",
            "--session",
            "--dest",
            APP_ID,
            "--object-path",
            "/eu/vanadrighem/conduit",
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
    resource = Path(os.environ["CONDUIT_TEST_RESOURCE"])
    schema = Path(os.environ["CONDUIT_TEST_SCHEMA"])

    with tempfile.TemporaryDirectory(
        prefix="conduit-search-provider-", ignore_cleanup_errors=True
    ) as temporary:
        root = Path(temporary)
        shutil.copy2(schema, root / schema.name)
        subprocess.run(
            ["glib-compile-schemas", "--strict", str(root)],
            check=True,
            capture_output=True,
            text=True,
        )
        state = root / "cache" / APP_ID / "state"
        state.mkdir(parents=True)
        workspace_key = "a" * 64
        (state / "active-workspace").write_text(workspace_key, encoding="utf-8")
        # Seed the legacy cache format; the provider migrates it to SQLite on
        # first use before answering the search request.
        (state / f"{workspace_key}.json").write_text(
            json.dumps(
                {
                    "version": 1,
                    "workspace_id": "Test Workspace",
                    "conversations": [
                        {
                            "id": "C_TEST",
                            "name": "general",
                            "is_channel": True,
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        environment = os.environ.copy()
        environment.update(
            {
                "CONDUIT_RESOURCE_PATH": str(resource),
                "CONDUIT_TEST_WORKSPACE": "1",
                "CONDUIT_TEST_OPEN_TARGET_FILE": str(root / "opened-target.json"),
                "GSETTINGS_SCHEMA_DIR": str(root),
                "XDG_CACHE_HOME": str(root / "cache"),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_DATA_HOME": str(root / "data"),
            }
        )
        subprocess.run(
            [
                "dbus-update-activation-environment",
                "CONDUIT_RESOURCE_PATH",
                "CONDUIT_TEST_WORKSPACE",
                "CONDUIT_TEST_OPEN_TARGET_FILE",
                "GSETTINGS_SCHEMA_DIR",
                "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
            ],
            env=environment,
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        )

        # Do not start Conduit directly: the first provider call must exercise
        # the installed D-Bus activation contract.
        initial = call(environment, "GetInitialResultSet", "['gen']")
        result_ids = re.findall(r"'([^']+)'", initial)
        assert len(result_ids) == 1, initial
        result_id = result_ids[0]
        assert "C_TEST" not in result_id

        refined = call(
            environment,
            "GetSubsearchResultSet",
            f"['{result_id}']",
            "['general']",
        )
        assert result_id in refined

        metadata = call(environment, "GetResultMetas", f"['{result_id}']")
        assert "#general" in metadata
        assert "Public channel" in metadata

        activated = call(
            environment,
            "ActivateResult",
            result_id,
            "['general']",
            "uint32 0",
        )
        assert activated.strip() == "()"
        target_path = root / "opened-target.json"
        wait_until(target_path.exists)
        assert json.loads(target_path.read_text(encoding="utf-8")) == {
            "workspace_id": "Test Workspace",
            "channel_id": "C_TEST",
        }

        # Closing a window with manually parented composer popovers must not
        # wedge GtkTextView disposal in a repeated warning loop.
        quit_application(environment)
        wait_until(lambda: not bus_name_owned(environment), timeout=10)


if __name__ == "__main__":
    main()
