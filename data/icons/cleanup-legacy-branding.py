#!/usr/bin/python3
"""Remove obsolete Conduit icons left behind by older Meson installs."""

from __future__ import annotations

import os
from pathlib import Path
import sys


APPLICATION_ID = "eu.vanadrighem.conduit"
LEGACY_ICON_PATHS = (
    Path(f"icons/hicolor/scalable/apps/{APPLICATION_ID}.svg"),
    Path(f"icons/hicolor/symbolic/apps/{APPLICATION_ID}-symbolic.svg"),
    Path(f"icons/hicolor/128x128/apps/{APPLICATION_ID}-about.png"),
    Path(f"icons/hicolor/256x256/apps/{APPLICATION_ID}-about.png"),
    Path(f"icons/hicolor/512x512/apps/{APPLICATION_ID}-about.png"),
)


def destdir_root(install_prefix: Path, destdir_prefix: Path) -> Path | None:
    if destdir_prefix == install_prefix:
        return None

    prefix_parts = install_prefix.relative_to("/").parts
    if not prefix_parts:
        return destdir_prefix
    if destdir_prefix.parts[-len(prefix_parts) :] != prefix_parts:
        raise RuntimeError("Meson install prefix does not match DESTDIR prefix")
    return Path(*destdir_prefix.parts[: -len(prefix_parts)])


def installed_data_root(datadir: Path) -> Path:
    install_prefix = Path(os.environ["MESON_INSTALL_PREFIX"])
    destdir_prefix = Path(os.environ["MESON_INSTALL_DESTDIR_PREFIX"])
    if not datadir.is_absolute():
        return destdir_prefix / datadir

    staging_root = destdir_root(install_prefix, destdir_prefix)
    if staging_root is None:
        return datadir
    return staging_root / datadir.relative_to("/")


def remove_legacy_icons(data_root: Path) -> None:
    for relative_path in LEGACY_ICON_PATHS:
        icon = data_root / relative_path
        if icon.is_file() or icon.is_symlink():
            icon.unlink()
        elif icon.exists():
            raise RuntimeError(f"refusing to remove non-file legacy icon: {icon}")


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: cleanup-legacy-branding.py DATADIR")
    remove_legacy_icons(installed_data_root(Path(sys.argv[1])))


if __name__ == "__main__":
    main()
