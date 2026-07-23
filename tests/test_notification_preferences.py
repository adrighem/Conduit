#!/usr/bin/env python3

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

from gi.repository import Gio


EXPECTED_KEYS = {
    "notifications-enabled-v1": ("b", "true"),
    "notifications-direct-messages-v1": ("b", "true"),
    "notifications-mentions-and-names-v1": ("b", "true"),
    "notifications-thread-replies-v1": ("b", "true"),
    "notifications-names-and-aliases-v1": ("as", "[]"),
    "notifications-keywords-v1": ("as", "[]"),
}


def main() -> None:
    schema_path = Path(sys.argv[1])
    root = ET.parse(schema_path).getroot()
    schema = root.find("./schema[@id='eu.vanadrighem.conduit']")
    assert schema is not None, "application GSettings schema is missing"

    notification_keys = {
        key.attrib["name"]: key
        for key in schema.findall("key")
        if key.attrib["name"].startswith("notifications-")
    }
    assert set(notification_keys) == set(EXPECTED_KEYS)

    for name, (expected_type, expected_default) in EXPECTED_KEYS.items():
        key = notification_keys[name]
        assert key.attrib["type"] == expected_type
        default = key.findtext("default")
        assert default is not None
        assert default.strip() == expected_default
        assert key.findtext("summary", "").strip()
        assert key.findtext("description", "").strip()

    with tempfile.TemporaryDirectory(prefix="conduit-notification-schema-") as temporary:
        schema_directory = Path(temporary)
        shutil.copy2(schema_path, schema_directory / schema_path.name)
        subprocess.run(
            ["glib-compile-schemas", "--strict", str(schema_directory)],
            check=True,
            capture_output=True,
            text=True,
        )
        source = Gio.SettingsSchemaSource.new_from_directory(
            str(schema_directory),
            Gio.SettingsSchemaSource.get_default(),
            False,
        )
        compiled = source.lookup("eu.vanadrighem.conduit", False)
        assert compiled is not None
        settings = Gio.Settings.new_full(
            compiled,
            Gio.memory_settings_backend_new(),
            None,
        )

        boolean_keys = [
            name for name, (value_type, _) in EXPECTED_KEYS.items() if value_type == "b"
        ]
        list_keys = [
            name for name, (value_type, _) in EXPECTED_KEYS.items() if value_type == "as"
        ]
        assert all(settings.get_boolean(name) for name in boolean_keys)
        assert all(settings.get_strv(name) == [] for name in list_keys)

        changes: list[str] = []
        settings.connect("changed", lambda _, key: changes.append(key))
        master_key = "notifications-enabled-v1"
        direct_message_key = "notifications-direct-messages-v1"
        action = Gio.SimpleAction.new("desktop-notifications", None)
        settings.bind(
            master_key,
            action,
            "enabled",
            Gio.SettingsBindFlags.DEFAULT,
        )
        assert action.get_enabled() is True

        assert settings.set_boolean(master_key, False)
        assert settings.get_boolean(master_key) is False
        assert action.get_enabled() is False
        assert changes == [master_key]

        action.set_enabled(True)
        assert settings.get_boolean(master_key) is True
        assert changes[-1] == master_key

        assert settings.set_boolean(direct_message_key, False)
        assert settings.set_boolean(master_key, False)
        assert settings.get_boolean(direct_message_key) is False
        assert settings.set_boolean(master_key, True)
        assert settings.get_boolean(direct_message_key) is False

        values = ["incident review, today", "on-call!", "naïve café"]
        assert settings.set_strv(list_keys[0], values)
        assert settings.get_strv(list_keys[0]) == values
        assert changes[-1] == list_keys[0]


if __name__ == "__main__":
    main()
