#!/usr/bin/python3
"""Validate that every product surface uses the current Conduit artwork."""

from __future__ import annotations

import gi
import json
from pathlib import Path
import shutil
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


def verify_install_plan(build_root: Path) -> None:
    plan = json.loads(
        (build_root / "meson-info" / "intro-install_plan.json").read_text(
            encoding="utf-8"
        )
    )
    destinations = {
        entry["destination"]
        for section in plan.values()
        for entry in section.values()
    }
    assert (
        f"{{datadir}}/applications/{APPLICATION_ID}.desktop" in destinations
    )
    for size in ICON_SIZES:
        assert (
            f"{{datadir}}/icons/hicolor/{size}x{size}/apps/{APPLICATION_ID}.png"
            in destinations
        )
    for relative_path in LEGACY_ICON_PATHS:
        assert f"{{datadir}}/{relative_path}" not in destinations


def minimal_environment(home: Path) -> dict[str, str]:
    home.mkdir()
    return {
        "CI": "true",
        "HOME": str(home),
        "LANG": "C.UTF-8",
        "PATH": "/usr/local/bin:/usr/bin:/bin",
    }


def verify_generated_install_order(
    build_root: Path,
    configured_datadir: str,
) -> None:
    with tempfile.TemporaryDirectory(prefix="conduit-install-order-") as temporary:
        temporary_root = Path(temporary)
        copied_build = temporary_root / "build"
        (copied_build / "meson-private").mkdir(parents=True)
        (copied_build / "meson-logs").mkdir()
        shutil.copy2(
            build_root / "meson-private" / "install.dat",
            copied_build / "meson-private" / "install.dat",
        )
        for directory in ("data", "src"):
            (copied_build / directory).symlink_to(
                build_root / directory,
                target_is_directory=True,
            )

        environment = minimal_environment(temporary_root / "home")
        meson = shutil.which("meson", path=environment["PATH"])
        assert meson is not None
        dry_run = subprocess.run(
            [
                meson,
                "install",
                "-C",
                str(copied_build),
                "--no-rebuild",
                "--dry-run",
            ],
            env=environment,
            capture_output=True,
            text=True,
        )
        assert dry_run.returncode == 0, dry_run.stderr
        script_lines = [
            line
            for line in dry_run.stdout.splitlines()
            if line.startswith("Running custom install script")
        ]
        cleanup_index = next(
            index
            for index, line in enumerate(script_lines)
            if "cleanup-legacy-branding.py" in line
        )
        cache_index = next(
            index
            for index, line in enumerate(script_lines)
            if "update-icon-cache" in line
        )
        assert cleanup_index < cache_index
        assert configured_datadir in script_lines[cleanup_index]


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


def verify_upgrade_cleanup(
    root: Path,
    configured_prefix: str,
    configured_datadir: str,
    *,
    use_destdir: bool,
    use_absolute_datadir: bool = False,
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
        if use_destdir:
            install_prefix = configured_prefix
            install_datadir = configured_datadir
            data_root = staged_data_root(
                staging_root,
                install_prefix,
                install_datadir,
            )
        else:
            install_prefix = str(staging_root / "installed-prefix")
            if use_absolute_datadir:
                install_datadir = str(staging_root / "absolute-data")
                data_root = Path(install_datadir)
            else:
                install_datadir = "share"
                data_root = Path(install_prefix) / install_datadir
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

        fixture_source = staging_root / "fixture-source"
        fixture_build = staging_root / "fixture-build"
        fixture_source.mkdir()
        escaped_cleanup_script = str(cleanup_script).replace("\\", "\\\\").replace(
            "'", "\\'"
        )
        (fixture_source / "meson.build").write_text(
            f"""project('branding-cleanup-fixture')
cleanup_python = import('python').find_installation('python3')
meson.add_install_script(
  cleanup_python,
  '{escaped_cleanup_script}',
  get_option('datadir'),
)
""",
            encoding="utf-8",
        )

        install_environment = minimal_environment(staging_root / "home")
        meson = shutil.which("meson", path=install_environment["PATH"])
        assert meson is not None
        setup = subprocess.run(
            [
                meson,
                "setup",
                str(fixture_build),
                str(fixture_source),
                f"--prefix={install_prefix}",
                f"-Ddatadir={install_datadir}",
            ],
            env=install_environment,
            capture_output=True,
            text=True,
        )
        assert setup.returncode == 0, setup.stderr

        for _ in range(2):
            install_command = [
                meson,
                "install",
                "-C",
                str(fixture_build),
            ]
            if use_destdir:
                install_command.extend(["--destdir", str(staging_root)])
            install_command.extend(["--no-rebuild", "--quiet"])
            install = subprocess.run(
                install_command,
                env=install_environment,
                capture_output=True,
                text=True,
            )
            assert install.returncode == 0, install.stderr

        for relative_path in LEGACY_ICON_PATHS:
            assert not (data_root / relative_path).exists(), relative_path
        assert unrelated_icon.read_text(encoding="utf-8") == "unrelated artwork"
        assert current_icon.read_text(encoding="utf-8") == "current Conduit artwork"


def main() -> None:
    root = Path(sys.argv[1])
    build_root = Path(sys.argv[2])
    configured_prefix = sys.argv[3]
    configured_datadir = sys.argv[4]
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

    verify_install_plan(build_root)
    verify_generated_install_order(build_root, configured_datadir)
    verify_upgrade_cleanup(
        root,
        configured_prefix,
        configured_datadir,
        use_destdir=True,
    )
    verify_upgrade_cleanup(
        root,
        configured_prefix,
        "/opt/conduit-branding-test",
        use_destdir=True,
    )
    verify_upgrade_cleanup(
        root,
        configured_prefix,
        configured_datadir,
        use_destdir=False,
    )
    verify_upgrade_cleanup(
        root,
        configured_prefix,
        configured_datadir,
        use_destdir=False,
        use_absolute_datadir=True,
    )


if __name__ == "__main__":
    main()
