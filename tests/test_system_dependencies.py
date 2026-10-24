#!/usr/bin/env python3
"""Keep system build dependencies in sync with the Linux -sys crates.

Every *-sys crate in the default Linux build must be classified below, and
each system library it links must be installed by CI, the release workflow,
and the Debian and RPM packages.
"""

import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-linux-gnu"

# -sys crate -> (pkg-config module, Debian/Ubuntu package, Fedora package).
SYSTEM_LIBRARIES: dict[str, tuple[str, str, str]] = {
    "gtk4-sys": ("gtk4", "libgtk-4-dev", "gtk4-devel"),
    "libadwaita-sys": ("libadwaita-1", "libadwaita-1-dev", "libadwaita-devel"),
    "libdbus-sys": ("dbus-1", "libdbus-1-dev", "dbus-devel"),
    "sourceview5-sys": (
        "gtksourceview-5",
        "libgtksourceview-5-dev",
        "gtksourceview5-devel",
    ),
}
# Crates whose library arrives with the dev packages above, is bundled, or
# comes from libc.
COVERED_SYS_CRATES = frozenset(
    {
        "aws-lc-sys",
        "gdk-pixbuf-sys",
        "gdk4-sys",
        "gdk4-wayland-sys",
        "gdk4-x11-sys",
        "gettext-sys",
        "gio-sys",
        "glib-sys",
        "gobject-sys",
        "graphene-sys",
        "gsk4-sys",
        "libsqlite3-sys",
        "linux-raw-sys",
        "pango-sys",
    }
)
DEBIAN_MANIFESTS = (
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    "packaging/debian/control",
)
FEDORA_MANIFESTS = (".github/workflows/release.yml",)
RPM_SPEC = "packaging/rpm/conduit.spec"


def linux_sys_crates() -> set[str]:
    tree = subprocess.run(
        [
            "cargo", "tree", "--locked",
            "--target", TARGET,
            "--edges", "normal,build",
            "--prefix", "none",
            "--format", "{p}",
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return {
        line.split()[0]
        for line in tree.splitlines()
        if line.split() and line.split()[0].endswith("-sys")
    }


def test_every_sys_crate_is_classified(crates: set[str]) -> None:
    unknown = crates - SYSTEM_LIBRARIES.keys() - COVERED_SYS_CRATES
    assert not unknown, (
        f"classify new -sys crates in {Path(__file__).name}: {sorted(unknown)}"
    )


def mentions_package(manifest: str, package: str) -> bool:
    text = (ROOT / manifest).read_text(encoding="utf-8")
    return re.search(rf"(?<![\w.-]){re.escape(package)}(?![\w.-])", text) is not None


def test_system_libraries_are_installed_everywhere(crates: set[str]) -> None:
    missing = []
    for crate in sorted(crates & SYSTEM_LIBRARIES.keys()):
        module, debian_package, fedora_package = SYSTEM_LIBRARIES[crate]
        missing += [
            f"{manifest}: {package}"
            for manifests, package in (
                (DEBIAN_MANIFESTS, debian_package),
                (FEDORA_MANIFESTS, fedora_package),
            )
            for manifest in manifests
            if not mentions_package(manifest, package)
        ]
        if f"pkgconfig({module})" not in (ROOT / RPM_SPEC).read_text(encoding="utf-8"):
            missing.append(f"{RPM_SPEC}: pkgconfig({module})")
    assert not missing, "missing system build dependencies:\n  " + "\n  ".join(missing)


def main() -> None:
    crates = linux_sys_crates()
    test_every_sys_crate_is_classified(crates)
    test_system_libraries_are_installed_everywhere(crates)
    print(f"system dependency checks passed ({len(crates)} -sys crates)")


if __name__ == "__main__":
    main()
