#!/usr/bin/python3
"""Validate that every product surface uses the current Conduit artwork."""

from __future__ import annotations

import gi
import os
from pathlib import Path
import subprocess
import sys
import tempfile

gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf


APPLICATION_ID = "eu.vanadrighem.conduit"
ICON_SIZES = (16, 24, 32, 48, 64, 128, 256, 512)
LEGACY_ICON_PATHS = (
    f"icons/hicolor/scalable/apps/{APPLICATION_ID}.svg",
    f"icons/hicolor/symbolic/apps/{APPLICATION_ID}-symbolic.svg",
    f"icons/hicolor/128x128/apps/{APPLICATION_ID}-about.png",
    f"icons/hicolor/256x256/apps/{APPLICATION_ID}-about.png",
    f"icons/hicolor/512x512/apps/{APPLICATION_ID}-about.png",
)


def png_dimensions(path: Path) -> tuple[int, int]:
    image = GdkPixbuf.Pixbuf.new_from_file(str(path))
    return image.get_width(), image.get_height()


def staged_data_root(
    staging_root: Path,
    configured_prefix: str,
    configured_datadir: str,
) -> Path:
    datadir = Path(configured_datadir)
    if datadir.is_absolute():
        return staging_root / datadir.relative_to("/")
    prefix = Path(configured_prefix)
    if prefix.is_absolute():
        prefix = prefix.relative_to("/")
    return staging_root / prefix / datadir


def staged_prefix_root(staging_root: Path, configured_prefix: str) -> Path:
    prefix = Path(configured_prefix)
    if prefix.is_absolute():
        prefix = prefix.relative_to("/")
    return staging_root / prefix


def verify_upgrade_cleanup(
    root: Path,
    configured_prefix: str,
    configured_datadir: str,
) -> None:
    cleanup_script = root / "data" / "icons" / "cleanup-legacy-branding.py"
    assert cleanup_script.exists(), "Meson needs an installed-branding cleanup hook"

    icons_meson = (root / "data" / "icons" / "meson.build").read_text(
        encoding="utf-8"
    )
    assert "cleanup-legacy-branding.py" in icons_meson
    assert "meson.add_install_script" in icons_meson

    with tempfile.TemporaryDirectory(prefix="conduit-branding-install-") as temporary:
        staging_root = Path(temporary)
        data_root = staged_data_root(
            staging_root,
            configured_prefix,
            configured_datadir,
        )
        for relative_path in LEGACY_ICON_PATHS:
            legacy_icon = data_root / relative_path
            legacy_icon.parent.mkdir(parents=True, exist_ok=True)
            legacy_icon.write_text("obsolete Conduit artwork", encoding="utf-8")

        unrelated_icon = (
            data_root / "icons/hicolor/scalable/apps/example.unrelated.svg"
        )
        unrelated_icon.write_text("unrelated artwork", encoding="utf-8")
        current_icon = (
            data_root
            / "icons"
            / "hicolor"
            / "256x256"
            / "apps"
            / f"{APPLICATION_ID}.png"
        )
        current_icon.write_text("current Conduit artwork", encoding="utf-8")

        install_environment = {
            "CI": "true",
            "HOME": str(staging_root / "home"),
            "LANG": "C.UTF-8",
            "PATH": "/usr/local/bin:/usr/bin:/bin",
            "MESON_INSTALL_DESTDIR_PREFIX": str(
                staged_prefix_root(staging_root, configured_prefix)
            ),
            "MESON_INSTALL_PREFIX": configured_prefix,
        }
        for _ in range(2):
            cleanup = subprocess.run(
                [
                    sys.executable,
                    str(cleanup_script),
                    configured_datadir,
                ],
                env=install_environment,
                capture_output=True,
                text=True,
            )
            assert cleanup.returncode == 0, cleanup.stderr

        for relative_path in LEGACY_ICON_PATHS:
            assert not (data_root / relative_path).exists(), relative_path
        assert unrelated_icon.read_text(encoding="utf-8") == "unrelated artwork"
        assert current_icon.read_text(encoding="utf-8") == "current Conduit artwork"


def main() -> None:
    root = Path(sys.argv[1])
    configured_prefix = sys.argv[2]
    configured_datadir = sys.argv[3]
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

    application = (root / "src" / "application.rs").read_text(encoding="utf-8")
    assert "const ABOUT_ICON_NAME: &str = config::APPLICATION_ID;" in application
    assert ".application_icon(ABOUT_ICON_NAME)" in application

    verify_upgrade_cleanup(root, configured_prefix, configured_datadir)


if __name__ == "__main__":
    main()
