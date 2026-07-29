#!/usr/bin/python3
"""Headless regression tests for Conduit's application keyboard shortcuts."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

SWITCHER_TITLE = "Switch conversation"
WEBKIT_SETTINGS = {
    "allow_file_access": False,
    "allow_universal_access": False,
    "html5_database": False,
    "html5_local_storage": True,
    "javascript": True,
    "media": True,
    "webaudio": False,
    "webgl": False,
    "zoom_text_only": True,
}


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


def focus_window(window_id: str) -> None:
    # Ask the window manager first so GTK receives normal activation state.
    # A newly mapped window or dialog transition can briefly leave the target
    # unmapped. Retry instead of issuing a direct X focus request that races it.
    deadline = time.monotonic() + 15.0
    while time.monotonic() < deadline:
        active = subprocess.run(
            ["xdotool", "getactivewindow"],
            capture_output=True,
            text=True,
        )
        if active.returncode == 0 and active.stdout.strip() == window_id:
            break
        try:
            activation = subprocess.run(
                ["xdotool", "windowactivate", "--sync", window_id],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=2,
            )
        except subprocess.TimeoutExpired:
            activation = None
        if activation is not None and activation.returncode != 0:
            time.sleep(0.1)
            continue
        time.sleep(0.1)
    else:
        raise AssertionError(f"window {window_id} did not become activatable")
    # Give GTK one main-loop iteration to apply the activation transition.
    time.sleep(0.1)


def press(window_id: str, *keys: str) -> None:
    focus_window(window_id)
    subprocess.run(["xdotool", "key", *keys], check=True)


def type_text(window_id: str, text: str) -> None:
    focus_window(window_id)
    subprocess.run(
        [
            "xdotool",
            "type",
            "--clearmodifiers",
            "--delay",
            "10",
            text,
        ],
        check=True,
    )


def clipboard_text() -> str:
    result = subprocess.run(
        ["xclip", "-selection", "clipboard", "-o"],
        capture_output=True,
        text=True,
    )
    if result.returncode == 0:
        return result.stdout
    if result.returncode == 1:
        # X11 briefly has no clipboard owner while Ctrl+C is being handled.
        # Let wait_until retry instead of treating that state as a failure.
        return ""
    result.check_returncode()
    return result.stdout


def composer_text(window_id: str) -> str:
    sentinel = f"__conduit_clipboard_{time.monotonic_ns()}__"
    subprocess.run(
        ["xclip", "-selection", "clipboard", "-i"],
        input=sentinel,
        text=True,
        check=True,
    )
    press(window_id, "ctrl+a", "ctrl+c")
    time.sleep(0.05)

    def copied_text() -> str | None:
        text = clipboard_text()
        return None if text == sentinel else text

    return wait_until(copied_text, timeout=5.0)


def replace_composer_text(window_id: str, text: str) -> None:
    press(window_id, "ctrl+a", "BackSpace")
    type_text(window_id, text)
    time.sleep(0.1)


def completion_state(path: Path, expected: dict) -> dict | None:
    if not path.exists():
        return None
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return None
    return state if state == expected else None


def wait_for_completion_ready(
    path: Path,
    target: str,
    kind: str,
    query: str,
    selected: str,
) -> None:
    deadline = time.monotonic() + 5.0
    last_state = None
    while time.monotonic() < deadline:
        if not path.exists():
            time.sleep(0.05)
            continue
        try:
            last_state = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            time.sleep(0.05)
            continue
        if (
            last_state.get("ready") == kind
            and last_state.get("query") == query
            and last_state.get("selected") == selected
            and last_state.get("count", 0) > 0
            and last_state.get("target") == target
        ):
            return
        time.sleep(0.05)
    raise AssertionError(
        "completion did not become ready: "
        f"expected {kind}/{query}/{selected}/{target}, last state was {last_state!r}"
    )


def verify_emoji_completion(window_id: str, target: str, state_path: Path) -> None:
    press(window_id, "ctrl+m")
    time.sleep(0.1)

    replace_composer_text(window_id, ":+1")
    wait_for_completion_ready(state_path, target, "emoji", "+1", "+1")
    press(window_id, "Return")
    wait_until(
        lambda: completion_state(
            state_path,
            {"emoji": "+1", "target": target, "webkit": WEBKIT_SETTINGS},
        )
    )
    assert composer_text(window_id) == ":+1:"

    replace_composer_text(window_id, ":sm")
    wait_for_completion_ready(state_path, target, "emoji", "sm", "smiley")
    press(window_id, "Return")
    wait_until(
        lambda: completion_state(
            state_path,
            {"emoji": "smiley", "target": target, "webkit": WEBKIT_SETTINGS},
        )
    )
    assert composer_text(window_id) == ":smiley:"

    replace_composer_text(window_id, ":sm")
    wait_for_completion_ready(state_path, target, "emoji", "sm", "smiley")
    press(window_id, "Down")
    wait_for_completion_ready(state_path, target, "emoji", "sm", "smile")
    press(window_id, "Return")
    wait_until(
        lambda: completion_state(
            state_path,
            {"emoji": "smile", "target": target, "webkit": WEBKIT_SETTINGS},
        )
    )
    assert composer_text(window_id) == ":smile:"

    replace_composer_text(window_id, ":sm")
    wait_for_completion_ready(state_path, target, "emoji", "sm", "smiley")
    press(window_id, "Escape", "Tab", "ctrl+m")
    time.sleep(0.1)
    assert composer_text(window_id) == ":sm"


def verify_person_completion(window_id: str, target: str, state_path: Path) -> None:
    press(window_id, "ctrl+m")
    time.sleep(0.1)

    replace_composer_text(window_id, "@gra")
    wait_for_completion_ready(state_path, target, "mention", "gra", "UGRACE")
    press(window_id, "Tab")
    wait_until(
        lambda: completion_state(
            state_path,
            {
                "mention": "UGRACE",
                "serialized": "<@UGRACE> ",
                "target": target,
                "webkit": WEBKIT_SETTINGS,
            },
        ),
        timeout=5.0,
    )
    assert composer_text(window_id) == "@Grace Hopper "

    press(window_id, "Home", "Right", "Delete", "End")
    type_text(window_id, "@ada")
    wait_for_completion_ready(state_path, target, "mention", "ada", "UADA")
    press(window_id, "Tab")
    wait_until(
        lambda: completion_state(
            state_path,
            {
                "mention": "UADA",
                "serialized": "@race Hopper <@UADA> ",
                "target": target,
                "webkit": WEBKIT_SETTINGS,
            },
        ),
        timeout=5.0,
    )
    assert composer_text(window_id) == "@race Hopper @Ada Lovelace "

    replace_composer_text(window_id, "@")
    wait_for_completion_ready(state_path, target, "mention", "", "UADA")
    press(window_id, "Down")
    wait_for_completion_ready(state_path, target, "mention", "", "UGRACE")
    press(window_id, "Return")
    wait_until(
        lambda: completion_state(
            state_path,
            {
                "mention": "UGRACE",
                "serialized": "<@UGRACE> ",
                "target": target,
                "webkit": WEBKIT_SETTINGS,
            },
        ),
        timeout=5.0,
    )
    assert composer_text(window_id) == "@Grace Hopper "

    replace_composer_text(window_id, "@ada")
    wait_for_completion_ready(state_path, target, "mention", "ada", "UADA")
    press(window_id, "Escape", "Tab", "ctrl+m")
    time.sleep(0.1)
    assert composer_text(window_id) == "@ada"


def verify_hydrated_person_draft(window_id: str) -> None:
    press(window_id, "ctrl+m")
    time.sleep(0.1)
    assert composer_text(window_id) == "Draft @Grace Hopper"


def stop_process(process: subprocess.Popen[str]) -> None:
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)

    if process.returncode not in (0, -15):
        _, stderr = process.communicate()
        raise AssertionError(f"Conduit exited with {process.returncode}:\n{stderr}")


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

        for thread_composer in (False, True):
            run_environment = environment.copy()
            target = "thread" if thread_composer else "message"
            completion_path = temporary_path / f"{target}-completion.json"
            run_environment["CONDUIT_TEST_COMPOSER_COMPLETION_FILE"] = str(
                completion_path
            )
            run_environment["CONDUIT_TEST_COMPOSER_HYDRATION"] = "1"
            if thread_composer:
                run_environment["CONDUIT_TEST_THREAD_COMPOSER"] = "1"
            process = subprocess.Popen(
                [str(binary)],
                env=run_environment,
                text=True,
                stderr=subprocess.PIPE,
            )
            try:
                window_id = wait_for_window(process)

                if not thread_composer:
                    for _ in range(2):
                        press(window_id, "ctrl+k")
                        switcher_id = wait_until(
                            lambda: next(
                                iter(visible_window_ids(SWITCHER_TITLE)), None
                            )
                        )
                        press(switcher_id, "Escape")
                        wait_until(
                            lambda: not visible_window_ids(SWITCHER_TITLE), timeout=10.0
                        )
                    assert process.poll() is None

                verify_hydrated_person_draft(window_id)
                verify_emoji_completion(window_id, target, completion_path)
                verify_person_completion(window_id, target, completion_path)
            finally:
                stop_process(process)


if __name__ == "__main__":
    main()
