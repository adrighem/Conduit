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
    assert manifest in ({}, {".": cargo["package"]["version"]}), (
        "the Release Please manifest must be empty before bootstrap or match "
        "the synchronized package version afterwards"
    )


def test_release_workflow_builds_and_publishes_all_assets() -> None:
    workflow = read(".github/workflows/release.yml")

    assert "googleapis/release-please-action@v4" in workflow
    assert "release_created:" in workflow
    assert "draft_tag:" in workflow
    assert "Select existing draft release" in workflow
    assert "steps.recovery.outputs.release_created" in workflow
    assert 'gh release view "$DRAFT_TAG" --json isDraft' in workflow
    assert "GH_REPO: ${{ github.repository }}" in workflow
    assert "needs.release-please.outputs.release_created == 'true'" in workflow
    assert "debian:trixie" in workflow
    assert "fedora:44" in workflow
    rpm_dependencies = workflow.split(
        "      - name: Install Fedora build dependencies", maxsplit=1
    )[1].split("      - name: Build RPM package", maxsplit=1)[0]
    assert re.search(r"\bgit\b", rpm_dependencies)
    assert rpm_dependencies.index("dnf install") < rpm_dependencies.index(
        "uses: actions/checkout@v4"
    )
    rpm_build = workflow.split("      - name: Build RPM package", maxsplit=1)[1].split(
        "      - uses: actions/upload-artifact@v4", maxsplit=1
    )[0]
    assert 'git config --global --add safe.directory "$GITHUB_WORKSPACE"' in rpm_build
    assert rpm_build.index("safe.directory") < rpm_build.index("git archive")
    assert "gst-inspect-1.0" not in workflow
    assert "libgstreamer" not in workflow
    assert "gstreamer1.0-" not in workflow
    assert "gstreamer1(" not in workflow
    assert "gstreamer1-plugins" not in workflow
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
    assert 'gh release edit "$TAG_NAME" --target "$RELEASE_SHA"' in workflow

    flatpak_validation = workflow.split("\n  validate-flatpak:", maxsplit=1)[1].split(
        "\n  publish-assets:", maxsplit=1
    )[0]
    publication = workflow.split("\n  publish-assets:", maxsplit=1)[1]
    assert "needs: [release-please, build-flatpak]" in flatpak_validation
    assert "actions/download-artifact@v4" in flatpak_validation
    assert "flatpak install --system --noninteractive --assumeyes" in flatpak_validation
    assert 'flatpak info --system --show-ref "$APP_ID"' in flatpak_validation
    assert 'flatpak run --system --command=sh "$APP_ID"' in flatpak_validation
    assert "if grep -Fq pulseaudio permissions.ini" in flatpak_validation
    assert "      - validate-flatpak" in publication


def test_release_build_tests_reuse_the_release_profile() -> None:
    source_build = read("src/meson.build")
    rpm_spec = read("packaging/rpm/conduit.spec")
    rpm_check = rpm_spec.split("%check", maxsplit=1)[1].split("%files", maxsplit=1)[0]

    assert "cargo_test_args += [cargo_release_arg]" in source_build
    assert "%meson_test" in rpm_check


def test_release_packages_disable_unavailable_native_huddle_stack() -> None:
    ci = read(".github/workflows/ci.yml")
    debian_control = read("packaging/debian/control")
    debian_rules = read("packaging/debian/rules")
    rpm_spec = read("packaging/rpm/conduit.spec")

    assert "--features native-media,screen-share,huddle-harness" in ci
    assert "-Dnative_media=enabled -Dscreen_share=enabled" in ci
    assert "gstreamer" not in debian_control
    assert "-Dnative_media=disabled" in debian_rules
    assert "-Dscreen_share=disabled" in debian_rules
    assert "-Dnative_media=enabled" not in debian_rules
    assert "-Dscreen_share=enabled" not in debian_rules
    assert "gstreamer" not in rpm_spec
    assert "-Dnative_media=disabled" in rpm_spec
    assert "-Dscreen_share=disabled" in rpm_spec
    assert "-Dnative_media=enabled" not in rpm_spec
    assert "-Dscreen_share=enabled" not in rpm_spec


def test_release_flatpak_uses_current_checkout_without_debug_logging() -> None:
    manifest = json.loads(read(f"packaging/flatpak/{APP_ID}.json"))

    assert manifest["id"] == APP_ID
    assert manifest["runtime"] == "org.gnome.Platform"
    assert manifest["runtime-version"] == "50"
    assert manifest["command"] == "conduit"
    assert "RUST_LOG" not in manifest.get("build-options", {}).get("env", {})
    assert "--socket=pulseaudio" not in manifest["finish-args"]

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
    assert "-Dnative_media=disabled" in conduit["config-opts"]
    assert "-Dscreen_share=disabled" in conduit["config-opts"]
    assert "-Dnative_media=enabled" not in conduit["config-opts"]
    assert "-Dscreen_share=enabled" not in conduit["config-opts"]


def test_unused_direct_dependencies_are_absent_from_the_lockfile() -> None:
    cargo = tomllib.loads(read("Cargo.toml"))
    lockfile = tomllib.loads(read("Cargo.lock"))
    locked_names = {package["name"] for package in lockfile["package"]}

    for dependency in ("pulldown-cmark", "slack-blocks", "slack-morphism"):
        assert dependency not in cargo["dependencies"]
        assert dependency not in locked_names


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
        test_release_packages_disable_unavailable_native_huddle_stack,
        test_release_flatpak_uses_current_checkout_without_debug_logging,
        test_unused_direct_dependencies_are_absent_from_the_lockfile,
        test_flatpak_cargo_sources_match_the_lockfile,
    ]
    for test in tests:
        test()
    print(f"release automation checks passed ({len(tests)} tests)")


if __name__ == "__main__":
    main()
