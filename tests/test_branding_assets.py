#!/usr/bin/python3
"""Validate that every product surface uses the current Conduit artwork."""

from __future__ import annotations

import gi
from pathlib import Path
import sys

gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf


APPLICATION_ID = "eu.vanadrighem.conduit"
ICON_SIZES = (16, 24, 32, 48, 64, 128, 256, 512)


def png_dimensions(path: Path) -> tuple[int, int]:
    image = GdkPixbuf.Pixbuf.new_from_file(str(path))
    return image.get_width(), image.get_height()


def main() -> None:
    root = Path(sys.argv[1])
    branding = root / "data" / "branding" / "conduit.png"
    assert png_dimensions(branding) == (1024, 1024)

    icons_root = root / "data" / "icons" / "hicolor"
    for size in ICON_SIZES:
        icon = icons_root / f"{size}x{size}" / "apps" / f"{APPLICATION_ID}.png"
        assert png_dimensions(icon) == (size, size)

    desktop = (root / "data" / f"{APPLICATION_ID}.desktop.in").read_text(
        encoding="utf-8"
    )
    assert f"Icon={APPLICATION_ID}" in desktop

    resources = (root / "src" / "conduit.gresource.xml").read_text(encoding="utf-8")
    for size in ICON_SIZES:
        assert f"icons/hicolor/{size}x{size}/apps/{APPLICATION_ID}.png" in resources


if __name__ == "__main__":
    main()
