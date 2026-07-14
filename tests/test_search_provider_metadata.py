#!/usr/bin/python3
"""Validate GNOME Shell search-provider packaging metadata."""

from __future__ import annotations

import configparser
from pathlib import Path
import sys


def read_ini(path: Path) -> configparser.ConfigParser:
    parser = configparser.ConfigParser(interpolation=None)
    parser.optionxform = str
    with path.open(encoding="utf-8") as source:
        parser.read_file(source)
    return parser


def main() -> None:
    data = Path(sys.argv[1])
    provider = read_ini(data / "eu.vanadrighem.conduit.search-provider.ini")
    section = provider["Shell Search Provider"]
    assert section["DesktopId"] == "eu.vanadrighem.conduit.desktop"
    assert section["BusName"] == "eu.vanadrighem.conduit"
    assert section["ObjectPath"] == "/eu/vanadrighem/conduit/SearchProvider"
    assert section["Version"] == "2"

    service = read_ini(data / "eu.vanadrighem.conduit.service.in")["D-BUS Service"]
    assert service["Name"] == section["BusName"]
    assert service["Exec"].endswith("/conduit --gapplication-service")

    desktop = read_ini(data / "eu.vanadrighem.conduit.desktop.in")["Desktop Entry"]
    assert desktop["Type"] == "Application"
    assert desktop["Icon"] == "eu.vanadrighem.conduit"

    meson = (data / "meson.build").read_text(encoding="utf-8")
    assert "eu.vanadrighem.conduit.search-provider.ini" in meson
    assert "'gnome-shell' / 'search-providers'" in meson


if __name__ == "__main__":
    main()
