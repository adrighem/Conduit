#!/usr/bin/env python3

import json
import re
import sys
import tomllib
from pathlib import Path
from xml.etree import ElementTree


ROOT = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
APP_ID = "eu.vanadrighem.conduit"


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def test_release_versions_are_synchronized() -> None:
    config = json.loads(read("release-please-config.json"))
    manifest = json.loads(read(".release-please-manifest.json"))
    cargo = tomllib.loads(read("Cargo.toml"))
    meson = read("meson.build")
    metainfo = ElementTree.parse(
        ROOT / "data" / f"{APP_ID}.metainfo.xml.in"
    ).getroot()

    assert config["release-type"] == "rust"
    assert config["include-component-in-tag"] is False
    assert config["include-v-in-tag"] is True
    assert config["bootstrap-sha"] == "d634d8bebffda092c30470f4de257ddc21a9360f"
    assert manifest == {}, "the first Release Please run must create v0.1.0"

    extra_files = config["packages"]["."]["extra-files"]
    assert {entry["path"] for entry in extra_files} == {
        "meson.build",
        f"data/{APP_ID}.metainfo.xml.in",
    }
    assert "x-release-please-version" in meson
    assert "x-release-please-version-date" in read(
        f"data/{APP_ID}.metainfo.xml.in"
    )

    meson_version = re.search(r"version:\s*'([^']+)'", meson)
    appstream_release = metainfo.find("./releases/release")
    assert meson_version is not None
    assert appstream_release is not None
    assert cargo["package"]["version"] == meson_version.group(1)
    assert cargo["package"]["version"] == appstream_release.attrib["version"]


def test_release_workflow_builds_and_publishes_all_assets() -> None:
    workflow = read(".github/workflows/release.yml")

    assert "googleapis/release-please-action@v4" in workflow
    assert "release_created:" in workflow
    assert "needs.release-please.outputs.release_created == 'true'" in workflow
    assert "debian:trixie" in workflow
    assert "fedora:44" in workflow
    assert "gstreamer1.0-nice gstreamer1.0-plugins-bad" in workflow
    assert "gstreamer1.0-plugins-good" in workflow
    assert "libnice-gstreamer1" in workflow
    assert "gstreamer1-plugins-bad-free gstreamer1-plugins-base" in workflow
    assert "gstreamer1-plugins-good" in workflow
    assert 'CARGO_NET_RETRY: "10"' in workflow
    assert "RUSTUP_HOME: /opt/rustup" in workflow
    assert "RUSTUP_TOOLCHAIN: ${{ env.RUST_VERSION }}" in workflow
    assert 'rustup default "$RUST_VERSION"' not in workflow
    assert "dpkg-query --showformat='${Version}'" in workflow
    assert "rpm -q --qf '%{VERSION}-%{RELEASE}'" in workflow
    assert "/usr/share/conduit/conduit.gresource" in workflow
    assert "flatpak/flatpak-github-actions/flatpak-builder@v6" in workflow
    assert f"packaging/flatpak/{APP_ID}.json" in workflow
    assert "artifact-name:" not in workflow
    assert "SHA256SUMS" in workflow
    assert "gh release upload" in workflow


def test_release_build_tests_reuse_the_release_profile() -> None:
    source_build = read("src/meson.build")
    debian_control = read("packaging/debian/control")
    rpm_spec = read("packaging/rpm/conduit.spec")
    rpm_check = rpm_spec.split("%check", maxsplit=1)[1].split("%files", maxsplit=1)[0]

    assert "cargo_test_args += [cargo_release_arg]" in source_build
    assert "gstreamer1.0-nice," in debian_control
    assert "gstreamer1.0-plugins-bad," in debian_control
    assert "gstreamer1.0-plugins-base," in debian_control
    assert "gstreamer1.0-plugins-good," in debian_control
    assert "%meson_test" in rpm_check
    assert "BuildRequires:  gstreamer1(element-nicesrc)" in rpm_spec
    assert "BuildRequires:  gstreamer1-plugins-bad-free" in rpm_spec
    assert "BuildRequires:  gstreamer1-plugins-base" in rpm_spec
    assert "BuildRequires:  gstreamer1-plugins-good" in rpm_spec


def test_release_flatpak_uses_current_checkout_without_debug_logging() -> None:
    manifest = json.loads(read(f"packaging/flatpak/{APP_ID}.json"))

    assert manifest["id"] == APP_ID
    assert manifest["runtime"] == "org.gnome.Platform"
    assert manifest["runtime-version"] == "50"
    assert manifest["command"] == "conduit"
    assert "RUST_LOG" not in manifest.get("build-options", {}).get("env", {})

    conduit = next(
        module for module in manifest["modules"] if module["name"] == "conduit"
    )
    assert conduit["sources"][0] == {"type": "dir", "path": "../.."}
    assert "cargo-sources.json" in conduit["sources"]
    assert any(
        source.get("type") == "shell"
        for source in conduit["sources"]
        if isinstance(source, dict)
    )
    assert "--libdir=lib" in conduit["config-opts"]
    assert "-Dnative_media=enabled" in conduit["config-opts"]


def test_flatpak_cargo_sources_match_the_lockfile() -> None:
    lockfile = tomllib.loads(read("Cargo.lock"))
    sources = json.loads(read("packaging/flatpak/cargo-sources.json"))
    archives = {
        source["url"]: source["sha256"]
        for source in sources
        if source.get("type") == "archive"
    }
    expected = {
        f"https://static.crates.io/crates/{package['name']}/"
        f"{package['name']}-{package['version']}.crate": package["checksum"]
        for package in lockfile["package"]
        if package.get("source", "").startswith("registry+")
    }

    assert archives == expected
    assert any(
        source.get("dest-filename") == "config"
        and "replace-with = \"vendored-sources\"" in source.get("contents", "")
        for source in sources
    )


def main() -> None:
    tests = [
        test_release_versions_are_synchronized,
        test_release_workflow_builds_and_publishes_all_assets,
        test_release_build_tests_reuse_the_release_profile,
        test_release_flatpak_uses_current_checkout_without_debug_logging,
        test_flatpak_cargo_sources_match_the_lockfile,
    ]
    for test in tests:
        test()
    print(f"release automation checks passed ({len(tests)} tests)")


if __name__ == "__main__":
    main()
