#!/usr/bin/python3
"""Validate that every product surface uses the current Conduit artwork."""

from __future__ import annotations

from pathlib import Path
import struct
import sys


APPLICATION_ID = "eu.vanadrighem.conduit"
ICON_SIZES = (16, 24, 32, 48, 64, 128, 256, 512)


def png_dimensions(path: Path) -> tuple[int, int]:
    contents = path.read_bytes()
    assert contents[:8] == b"\x89PNG\r\n\x1a\n", f"{path} is not a PNG"
    assert contents[12:16] == b"IHDR", f"{path} has no PNG header"
    return struct.unpack(">II", contents[16:24])


def main() -> None:
    root = Path(sys.argv[1])
    branding = root / "data" / "branding" / "conduit.png"
    assert png_dimensions(branding) == (1024, 1024)

    icons_root = root / "data" / "icons" / "hicolor"
    for size in ICON_SIZES:
        icon = icons_root / f"{size}x{size}" / "apps" / f"{APPLICATION_ID}.png"
        assert png_dimensions(icon) == (size, size)

    assert not (icons_root / "scalable" / "apps" / f"{APPLICATION_ID}.svg").exists()
    assert sorted((root / "data" / "branding").iterdir()) == [branding]

    readme = (root / "README.md").read_text(encoding="utf-8")
    assert 'src="data/branding/conduit.png"' in readme

    desktop = (root / "data" / f"{APPLICATION_ID}.desktop.in").read_text(
        encoding="utf-8"
    )
    assert f"Icon={APPLICATION_ID}" in desktop

    metainfo = (root / "data" / f"{APPLICATION_ID}.metainfo.xml.in").read_text(
        encoding="utf-8"
    )
    assert "#996b50" in metainfo
    assert "#543b2f" in metainfo
    assert "#4a154b" not in metainfo
    assert "#2eb67d" not in metainfo

    resources = (root / "src" / "conduit.gresource.xml").read_text(encoding="utf-8")
    for size in ICON_SIZES:
        assert f"icons/hicolor/{size}x{size}/apps/{APPLICATION_ID}.png" in resources
    assert f"icons/hicolor/scalable/apps/{APPLICATION_ID}.svg" not in resources

    application = (root / "src" / "application.rs").read_text(encoding="utf-8")
    assert 'const ABOUT_ICON_NAME: &str = config::APPLICATION_ID;' in application
    assert ".application_icon(ABOUT_ICON_NAME)" in application


if __name__ == "__main__":
    main()
