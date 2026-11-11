#!/usr/bin/env python3

import copy
import json
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any
from xml.etree import ElementTree


ROOT = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
BUILD_ROOT = Path(sys.argv[2]) if len(sys.argv) > 2 else None
APP_ID = "eu.vanadrighem.conduit"
RELEASE_CONDITION = "needs.release-please.outputs.release_created == 'true'"
RELEASE_PAYLOAD_PATHS = (
    "bin/conduit",
    f"share/applications/{APP_ID}.desktop",
    "share/conduit/conduit.gresource",
    f"share/dbus-1/services/{APP_ID}.service",
    f"share/glib-2.0/schemas/{APP_ID}.gschema.xml",
    f"share/gnome-shell/search-providers/{APP_ID}.search-provider.ini",
    f"share/icons/hicolor/512x512/apps/{APP_ID}.png",
    f"share/metainfo/{APP_ID}.metainfo.xml",
)
NATIVE_MEDIA_DISABLED_OPTIONS = (
    "-Dnative_media=disabled",
    "-Dscreen_share=disabled",
)
HEADLESS_TESTS_DISABLED_OPTION = "-Dheadless_tests=disabled"
CHECKOUT_ACTION = "actions/checkout@9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0"
UPLOAD_ARTIFACT_ACTION = (
    "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a"
)
DOWNLOAD_ARTIFACT_ACTION = (
    "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c"
)
RELEASE_PR_FILES = {
    ".release-please-manifest.json",
    "CHANGELOG.md",
    "Cargo.lock",
    "Cargo.toml",
    f"data/{APP_ID}.metainfo.xml.in",
    "meson.build",
}
FLATPAK_CARGO_SOURCES_PATHS = (
    "packaging/flatpak/cargo-sources.json",
    "packaging/flatpak/flathub/cargo-sources.json",
)


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def active_text(document: str) -> str:
    return "\n".join(
        line for line in document.splitlines() if not line.lstrip().startswith("#")
    )


# These helpers verify the workflow's repository-specific shape. A workflow linter
# is still required to validate YAML syntax and GitHub expression semantics.
def workflow_jobs(document: str) -> dict[str, str]:
    lines = active_text(document).splitlines()
    try:
        jobs_start = lines.index("jobs:")
    except ValueError as error:
        raise AssertionError("release workflow has no jobs mapping") from error

    jobs: dict[str, str] = {}
    current_name: str | None = None
    current_lines: list[str] = []
    for line in lines[jobs_start + 1 :]:
        if line and not line.startswith(" "):
            break
        match = re.fullmatch(r"  ([A-Za-z0-9_-]+):", line)
        if match:
            if current_name is not None:
                jobs[current_name] = "\n".join(current_lines)
            current_name = match.group(1)
            current_lines = [line]
        elif current_name is not None:
            current_lines.append(line)
    if current_name is not None:
        jobs[current_name] = "\n".join(current_lines)

    assert jobs, "release workflow jobs mapping is empty"
    return jobs


def workflow_event(document: str, event: str) -> str:
    lines = active_text(document).splitlines()
    try:
        events_start = lines.index("on:")
    except ValueError as error:
        raise AssertionError("workflow has no event mapping") from error

    events: list[str] = []
    for line in lines[events_start + 1 :]:
        if line and not line.startswith(" "):
            break
        events.append(line)

    marker = f"  {event}:"
    try:
        event_start = events.index(marker)
    except ValueError as error:
        raise AssertionError(f"workflow has no {event!r} event") from error

    block = [marker]
    for line in events[event_start + 1 :]:
        if re.fullmatch(r"  [A-Za-z0-9_-]+:.*", line):
            break
        block.append(line)
    return "\n".join(block).rstrip()


def workflow_steps(job: str) -> list[str]:
    lines = job.splitlines()
    try:
        steps_start = lines.index("    steps:")
    except ValueError as error:
        raise AssertionError("workflow job has no steps sequence") from error

    steps: list[str] = []
    current: list[str] = []
    for line in lines[steps_start + 1 :]:
        if line.startswith("      - "):
            if current:
                steps.append("\n".join(current))
            current = [line]
        elif current:
            current.append(line)
    if current:
        steps.append("\n".join(current))

    assert steps, "workflow job has no steps"
    return steps


def step_containing(steps: list[str], needle: str) -> str:
    matches = [step for step in steps if needle in step]
    assert len(matches) == 1, f"expected one workflow step containing {needle!r}"
    return matches[0]


def job_needs(job: str) -> set[str]:
    lines = job.splitlines()
    for index, line in enumerate(lines):
        match = re.fullmatch(r"    needs:\s*(.*)", line)
        if not match:
            continue
        inline = match.group(1)
        if inline.startswith("[") and inline.endswith("]"):
            return {
                dependency.strip()
                for dependency in inline[1:-1].split(",")
                if dependency.strip()
            }
        if inline:
            return {inline.strip("'\"")}

        dependencies: set[str] = set()
        for item in lines[index + 1 :]:
            item_match = re.fullmatch(r"      - ([A-Za-z0-9_-]+)", item)
            if item_match:
                dependencies.add(item_match.group(1))
                continue
            if item and len(item) - len(item.lstrip()) <= 4:
                break
        return dependencies
    raise AssertionError("workflow job has no needs field")


def job_scalar(job: str, field: str) -> str:
    match = re.search(rf"^    {re.escape(field)}:\s*(.+)$", job, re.MULTILINE)
    assert match is not None, f"workflow job has no {field!r} field"
    return match.group(1).strip()


def assert_native_media_disabled(options: str | list[str], target: str) -> None:
    contents = active_text(options if isinstance(options, str) else "\n".join(options))
    for option in NATIVE_MEDIA_DISABLED_OPTIONS:
        assert option in contents, f"{target} must set {option}"
    assert "-Dnative_media=enabled" not in contents
    assert "-Dscreen_share=enabled" not in contents


def assert_headless_tests_disabled(options: str | list[str], target: str) -> None:
    contents = active_text(options if isinstance(options, str) else "\n".join(options))
    assert HEADLESS_TESTS_DISABLED_OPTION in contents, (
        f"{target} must disable the headless UI-test harness"
    )
    assert "-Dheadless_tests=enabled" not in contents


def assert_installed_payload_contract(job: str, target: str) -> None:
    for path in RELEASE_PAYLOAD_PATHS:
        assert path in job, f"{target} does not validate installed {path}"
    assert "for path in" in job
    assert 'test -e "$path"' in job or 'test -e "${app_root}/${path}"' in job
    assert 'grep -Fq "release version=' in job


def test_release_versions_are_synchronized() -> None:
    config = json.loads(read("release-please-config.json"))
    manifest = json.loads(read(".release-please-manifest.json"))
    cargo = tomllib.loads(read("Cargo.toml"))
    meson = read("meson.build")
    metainfo_text = read(f"data/{APP_ID}.metainfo.xml.in")
    metainfo = ElementTree.fromstring(metainfo_text)

    assert config["release-type"] == "rust"
    assert config["include-component-in-tag"] is False
    assert config["include-v-in-tag"] is True
    assert config["draft"] is True
    assert config["force-tag-creation"] is True

    extra_files = config["packages"]["."]["extra-files"]
    assert {entry["path"] for entry in extra_files} == {
        "meson.build",
        f"data/{APP_ID}.metainfo.xml.in",
    }
    assert "x-release-please-version" in meson
    assert "x-release-please-version-date" in metainfo_text

    meson_version = re.search(r"version:\s*'([^']+)'", meson)
    appstream_release = metainfo.find("./releases/release")
    screenshot_images = metainfo.findall("./screenshots/screenshot/image")
    assert meson_version is not None
    assert appstream_release is not None
    assert screenshot_images
    version = cargo["package"]["version"]
    assert version == meson_version.group(1)
    assert version == appstream_release.attrib["version"]

    expected_release_url = f"https://github.com/adrighem/Conduit/releases/tag/v{version}"
    assert appstream_release.findtext("url") == expected_release_url
    release_url_lines = [
        line for line in metainfo_text.splitlines() if 'type="details"' in line
    ]
    assert len(release_url_lines) == 1
    assert expected_release_url in release_url_lines[0]
    assert "</url>" in release_url_lines[0]
    assert "x-release-please-version" in release_url_lines[0]
    first_url_semver = re.search(r"\d+\.\d+\.\d+", release_url_lines[0])
    assert first_url_semver is not None and first_url_semver.group(0) == version

    raw_prefix = f"https://raw.githubusercontent.com/adrighem/Conduit/v{version}/"
    for image in screenshot_images:
        assert image.text is not None
        assert image.text == image.text.strip()
        assert image.text.startswith(raw_prefix)
        relative_path = image.text.removeprefix(raw_prefix)
        assert not relative_path.startswith("/")
        assert ".." not in Path(relative_path).parts
        assert relative_path.endswith(".png")
        assert (ROOT / relative_path).is_file(), (
            f"AppStream screenshot {relative_path!r} does not exist"
        )
    image_lines = [
        line for line in metainfo_text.splitlines() if re.search(r"<image\b", line)
    ]
    assert len(image_lines) == len(screenshot_images)
    for line in image_lines:
        assert "</image>" in line
        assert raw_prefix in line
        assert "x-release-please-version" in line
        first_semver = re.search(r"\d+\.\d+\.\d+", line)
        assert first_semver is not None and first_semver.group(0) == version
    assert manifest == {".": version}


def test_rust_toolchain_is_pinned_across_local_ci_and_release() -> None:
    toolchain = tomllib.loads(read("rust-toolchain.toml"))["toolchain"]
    channel = toolchain["channel"]
    assert re.fullmatch(r"\d+\.\d+\.\d+", channel)
    assert toolchain["profile"] == "minimal"
    assert set(toolchain["components"]) == {"clippy", "rustfmt"}

    ci_workflow = active_text(read(".github/workflows/ci.yml"))
    release_workflow = active_text(read(".github/workflows/release.yml"))
    assert f"dtolnay/rust-toolchain@{channel}" in ci_workflow
    assert "dtolnay/rust-toolchain@stable" not in ci_workflow
    assert f'RUST_VERSION: "{channel}"' in release_workflow


def test_local_meson_suite_enforces_strict_rust_lints() -> None:
    rust_build = read("src/meson.build")
    clippy_section = rust_build.split("if cargo_clippy_compatible", maxsplit=1)[1]
    lint_test = clippy_section.split("test('Rust strict lints'", maxsplit=1)[1]
    assert "'clippy'," in clippy_section
    assert "'--locked'," in clippy_section
    assert "'--all-targets'," in clippy_section
    assert "cargo_clippy_args += ['--features', cargo_features_arg]" in clippy_section
    assert "'-D', 'warnings'" in clippy_section
    assert "args: cargo_clippy_args," in lint_test
    assert "suite: 'rust-lint'" in lint_test


def test_local_meson_suite_enforces_rust_formatting() -> None:
    rust_build = read("src/meson.build")
    fmt_test = rust_build.split("test('Rust formatting'", maxsplit=1)[1].split(
        "\n)", maxsplit=1
    )[0]
    assert "'fmt'," in fmt_test
    assert "'--check'," in fmt_test
    assert "suite: 'rust-lint'" in fmt_test

    assert "--no-suite rust-lint" in read("packaging/debian/rules")
    assert "--no-suite rust-lint" in read("packaging/rpm/conduit.spec")
    if BUILD_ROOT is not None:
        configured_tests = json.loads(
            (BUILD_ROOT / "meson-info" / "intro-tests.json").read_text(
                encoding="utf-8"
            )
        )
        fmt_entries = [
            entry for entry in configured_tests if entry["name"] == "Rust formatting"
        ]
        assert len(fmt_entries) == 1
        assert "conduit:rust-lint" in fmt_entries[0]["suite"]
        assert "fmt" in fmt_entries[0]["cmd"]
        assert "--check" in fmt_entries[0]["cmd"]


def test_cargo_build_depends_on_compiled_resources() -> None:
    rust_build = read("src/meson.build")
    assert "conduit_resources = gnome.compile_resources" in rust_build
    cargo_build = rust_build.split("cargo_build = custom_target", 1)[1].split(
        "cargo_test_args", 1
    )[0]
    assert "depends: conduit_resources" in cargo_build


def test_release_workflow_builds_validates_and_publishes_all_assets() -> None:
    workflow = read(".github/workflows/release.yml")
    jobs = workflow_jobs(workflow)
    dependencies = {
        "build-deb": {"release-please"},
        "validate-deb": {"release-please", "build-deb"},
        "build-rpm": {"release-please"},
        "validate-rpm": {"release-please", "build-rpm"},
        "build-flatpak": {"release-please"},
        "validate-flatpak": {"release-please", "build-flatpak"},
        "publish-assets": {
            "release-please",
            "build-deb",
            "validate-deb",
            "build-rpm",
            "validate-rpm",
            "build-flatpak",
            "validate-flatpak",
        },
    }
    assert {"release-please", *dependencies} <= jobs.keys()
    for job_name, expected_needs in dependencies.items():
        assert job_needs(jobs[job_name]) == expected_needs
        assert job_scalar(jobs[job_name], "if") == RELEASE_CONDITION

    release_job = jobs["release-please"]
    release_steps = workflow_steps(release_job)
    release_action = step_containing(release_steps, "googleapis/release-please-action@")
    assert "config-file: release-please-config.json" in release_action
    assert "manifest-file: .release-please-manifest.json" in release_action
    recovery = step_containing(release_steps, "id: recovery")
    assert "inputs.draft_tag != ''" in recovery
    assert "GH_REPO: ${{ github.repository }}" in recovery
    assert 'gh release view "$DRAFT_TAG" --json isDraft' in recovery
    assert 'git/ref/tags/${DRAFT_TAG}' in recovery
    assert 'echo "sha=$release_sha"' in recovery
    assert 'echo "sha=$GITHUB_SHA"' not in recovery
    for output in ("release_created", "tag_name", "version", "sha"):
        output_mapping = re.search(rf"^      {output}:\s*(.+)$", release_job, re.MULTILINE)
        assert output_mapping is not None, f"release output {output!r} lacks recovery"
        mapping = output_mapping.group(1)
        assert f"steps.release.outputs.{output}" in mapping
        assert f"steps.recovery.outputs.{output}" in mapping
        assert f'echo "{output}=' in recovery

    for job_name in ("build-deb", "build-rpm", "build-flatpak"):
        build_steps = workflow_steps(jobs[job_name])
        checkout = step_containing(build_steps, CHECKOUT_ACTION)
        assert "ref: ${{ needs.release-please.outputs.sha }}" in checkout
        if job_name != "build-flatpak":
            step_containing(build_steps, UPLOAD_ARTIFACT_ACTION)

    rpm_steps = workflow_steps(jobs["build-rpm"])
    rpm_dependencies = step_containing(rpm_steps, "dnf install")
    rpm_checkout = step_containing(rpm_steps, CHECKOUT_ACTION)
    rpm_build = step_containing(rpm_steps, "git archive")
    assert re.search(r"\bgit\b", rpm_dependencies)
    assert rpm_steps.index(rpm_dependencies) < rpm_steps.index(rpm_checkout)
    assert rpm_steps.index(rpm_checkout) < rpm_steps.index(rpm_build)
    assert 'git config --global --add safe.directory "$GITHUB_WORKSPACE"' in rpm_build
    assert rpm_build.index("safe.directory") < rpm_build.index("git archive")

    flatpak_build = step_containing(
        workflow_steps(jobs["build-flatpak"]),
        "flatpak/flatpak-github-actions/flatpak-builder@",
    )
    assert f"manifest-path: packaging/flatpak/{APP_ID}.json" in flatpak_build

    for job_name in ("validate-deb", "validate-rpm", "validate-flatpak"):
        assert_installed_payload_contract(jobs[job_name], job_name)
        step_containing(workflow_steps(jobs[job_name]), DOWNLOAD_ARTIFACT_ACTION)

    for job_name in ("validate-deb", "validate-rpm"):
        validation = jobs[job_name]
        assert "ldd /usr/bin/conduit" in validation
        assert "readelf -d /usr/bin/conduit" in validation
        assert "glib-compile-schemas --strict --dry-run" in validation
        assert "desktop-file-validate" in validation
        assert "appstreamcli validate --no-net" in validation

    debian_validation = jobs["validate-deb"]
    assert "dpkg-query --showformat='${Version}'" in debian_validation
    rpm_validation = jobs["validate-rpm"]
    assert "rpm -q --qf '%{VERSION}-%{RELEASE}'" in rpm_validation

    flatpak_validation = jobs["validate-flatpak"]
    assert "flatpak install --system --noninteractive --assumeyes" in flatpak_validation
    assert 'flatpak info --system --show-ref "$APP_ID"' in flatpak_validation
    assert 'flatpak info --system --show-runtime "$APP_ID"' in flatpak_validation
    assert 'flatpak info --system --show-commit "$APP_ID"' in flatpak_validation
    assert "if grep -Fq pulseaudio permissions.ini" in flatpak_validation
    assert 'flatpak run --system --command=sh "$APP_ID"' in flatpak_validation

    publication = jobs["publish-assets"]
    step_containing(workflow_steps(publication), DOWNLOAD_ARTIFACT_ACTION)
    publish_step = step_containing(workflow_steps(publication), "gh release upload")
    assert "SHA256SUMS" in publish_step
    assert 'gh release edit "$TAG_NAME" --target "$RELEASE_SHA"' in publish_step


def test_generated_release_pull_requests_use_verified_dispatched_ci() -> None:
    ci_workflow = read(".github/workflows/ci.yml")
    pull_request = workflow_event(ci_workflow, "pull_request")
    ignored_paths = set(re.findall(r"^      - (.+)$", pull_request, re.MULTILINE))
    assert ignored_paths == {
        ".release-please-manifest.json",
        "CHANGELOG.md",
        f"data/{APP_ID}.metainfo.xml.in",
    }
    workflow_dispatch = workflow_event(ci_workflow, "workflow_dispatch")
    assert "      commit_sha:" in workflow_dispatch
    assert "        required: true" in workflow_dispatch
    assert "        type: string" in workflow_dispatch

    ci_job = workflow_jobs(ci_workflow)["build"]
    checkout = step_containing(workflow_steps(ci_job), CHECKOUT_ACTION)
    assert "ref: ${{ inputs.commit_sha || github.sha }}" in checkout
    assert "persist-credentials: false" in checkout

    release_workflow = read(".github/workflows/release.yml")
    release_events = active_text(release_workflow)
    assert "\n  push:" not in release_events
    workflow_run = workflow_event(release_workflow, "workflow_run")
    assert "    workflows: [CI]" in workflow_run
    assert "    types: [completed]" in workflow_run
    assert "    branches: [main]" in workflow_run
    release_jobs = workflow_jobs(release_workflow)
    release_job = release_jobs["release-please"]
    assert (
        job_scalar(release_job, "if")
        == "github.event_name == 'workflow_dispatch' || github.event.workflow_run.conclusion == 'success'"
    )
    assert "      actions: write" not in release_job
    assert "secrets.RELEASE_PLEASE_TOKEN" not in release_job
    assert "token: ${{ github.token }}" in release_job
    assert (
        "release_pr_created: ${{ steps.release.outputs.prs_created }}"
        in release_job
    )
    assert "release_pr: ${{ steps.release.outputs.pr }}" in release_job
    assert (
        "googleapis/release-please-action@"
        "5c625bfb5d1ff62eadeeb3772007f7f66fdcf071" in release_job
    )

    dispatch_job = release_jobs["validate-release-pr"]
    assert job_needs(dispatch_job) == {"release-please"}
    assert (
        job_scalar(dispatch_job, "if")
        == "needs.release-please.outputs.release_pr_created == 'true'"
    )
    permissions = dict(
        re.findall(r"^      ([A-Za-z-]+): (read|write)$", dispatch_job, re.MULTILINE)
    )
    assert permissions == {
        "actions": "write",
        "contents": "read",
        "pull-requests": "read",
    }
    dispatch = step_containing(
        workflow_steps(dispatch_job), "gh workflow run ci.yml"
    )
    expected_files = re.search(r"EXPECTED_FILES: >-\n\s+(\[.*\])", dispatch)
    assert expected_files is not None
    assert set(json.loads(expected_files.group(1))) == RELEASE_PR_FILES
    assert "RELEASE_PR: ${{ needs.release-please.outputs.release_pr }}" in dispatch
    assert "release-please--branches--main--components--conduit" in dispatch
    assert "app/github-actions" in dispatch
    assert "isCrossRepository" in dispatch
    assert "baseRefName" in dispatch
    assert "files" in dispatch
    assert "headRefOid" in dispatch
    assert '--argjson expected_files "$EXPECTED_FILES"' in dispatch
    assert "(([.files[].path] | sort) == ($expected_files | sort))" in dispatch
    assert "release_pr_sha=$(jq -er '.headRefOid'" in dispatch
    assert '--ref "$release_pr_branch"' in dispatch
    assert '-f commit_sha="$release_pr_sha"' in dispatch


def test_release_build_tests_reuse_the_release_profile() -> None:
    rpm_spec = read("packaging/rpm/conduit.spec")
    rpm_check = rpm_spec.split("%check", maxsplit=1)[1].split("%files", maxsplit=1)[0]

    if BUILD_ROOT is not None:
        build_options = json.loads(
            (BUILD_ROOT / "meson-info" / "intro-buildoptions.json").read_text(
                encoding="utf-8"
            )
        )
        buildtype = next(
            option["value"] for option in build_options if option["name"] == "buildtype"
        )
        configured_tests = json.loads(
            (BUILD_ROOT / "meson-info" / "intro-tests.json").read_text(
                encoding="utf-8"
            )
        )
        cargo_tests = [test for test in configured_tests if test["name"] == "Cargo tests"]
        assert len(cargo_tests) == 1
        assert ("--release" in cargo_tests[0]["cmd"]) == (buildtype == "release")
    assert "%meson_test" in rpm_check


def test_release_packages_disable_nonproduction_features() -> None:
    debian_rules = read("packaging/debian/rules")
    rpm_spec = read("packaging/rpm/conduit.spec")
    debian_configure = debian_rules.split(
        "override_dh_auto_configure:", maxsplit=1
    )[1].split("override_dh_auto_build:", maxsplit=1)[0]
    rpm_build = rpm_spec.split("%build", maxsplit=1)[1].split(
        "%install", maxsplit=1
    )[0]

    assert_native_media_disabled(debian_configure, "Debian package")
    assert_native_media_disabled(rpm_build, "RPM package")
    assert_headless_tests_disabled(debian_configure, "Debian package")
    assert_headless_tests_disabled(rpm_build, "RPM package")


def find_conduit_module(
    manifest: dict[str, Any], manifest_path: str
) -> dict[str, Any]:
    conduit_modules = [
        module for module in manifest["modules"] if module.get("name") == "conduit"
    ]
    assert len(conduit_modules) == 1, f"{manifest_path} has no conduit module"
    return conduit_modules[0]


def assert_flatpak_manifest_safety(
    manifest_path: str,
) -> tuple[dict[str, Any], dict[str, Any]]:
    manifest: dict[str, Any] = json.loads(read(manifest_path))

    assert manifest["id"] == APP_ID
    assert manifest["runtime"] == "org.gnome.Platform"
    assert manifest["runtime-version"] == "50"
    assert manifest["command"] == "conduit"
    assert "--socket=pulseaudio" not in manifest["finish-args"]
    assert "--filesystem=xdg-download:ro" not in manifest["finish-args"]
    assert not any(
        argument == "--env=RUST_LOG"
        or argument.startswith("--env=RUST_LOG=")
        for argument in manifest["finish-args"]
    )

    conduit = find_conduit_module(manifest, manifest_path)
    assert "cargo-sources.json" in conduit["sources"]
    assert any(
        source.get("type") == "shell"
        and "cp -vf cargo/config .cargo/config.toml" in source.get("commands", [])
        for source in conduit["sources"]
        if isinstance(source, dict)
    )
    assert "--libdir=lib" in conduit["config-opts"]
    assert "--buildtype=release" in conduit["config-opts"]
    assert (
        conduit.get("build-options", {}).get("env", {}).get("CARGO_NET_OFFLINE")
        == "true"
    )
    assert_native_media_disabled(conduit["config-opts"], manifest_path)
    assert_headless_tests_disabled(conduit["config-opts"], manifest_path)
    for scope in (manifest, *manifest["modules"]):
        build_options = scope.get("build-options", {})
        environment = build_options.get("env", {})
        build_args = build_options.get("build-args", [])
        assert "RUST_LOG" not in environment
        assert "--share=network" not in build_args
    return manifest, conduit


def test_release_and_flathub_flatpak_manifests_are_hardened() -> None:
    release_manifest, conduit = assert_flatpak_manifest_safety(
        f"packaging/flatpak/{APP_ID}.json"
    )
    assert conduit["sources"][0] == {"type": "dir", "path": "../.."}

    flathub_manifest, flathub_conduit = assert_flatpak_manifest_safety(
        f"packaging/flatpak/flathub/{APP_ID}.json"
    )
    archive = flathub_conduit["sources"][0]
    assert archive["type"] == "archive"
    assert re.fullmatch(
        r"https://github\.com/adrighem/Conduit/archive/refs/tags/v\d+\.\d+\.\d+\.tar\.gz",
        archive["url"],
    )
    assert re.fullmatch(r"[0-9a-f]{64}", archive["sha256"])

    release_normalized = copy.deepcopy(release_manifest)
    flathub_normalized = copy.deepcopy(flathub_manifest)
    find_conduit_module(
        release_normalized, f"packaging/flatpak/{APP_ID}.json"
    )["sources"][0] = None
    find_conduit_module(
        flathub_normalized, f"packaging/flatpak/flathub/{APP_ID}.json"
    )["sources"][0] = None
    assert release_normalized == flathub_normalized


def expected_crates_from_lockfile(lockfile_toml: str) -> dict[str, str]:
    lockfile = tomllib.loads(lockfile_toml)
    expected: dict[str, str] = {}
    for package in lockfile["package"]:
        source = package.get("source")
        if source is None:
            continue
        assert source == "registry+https://github.com/rust-lang/crates.io-index", (
            f"unexpected non-crates.io dependency source for {package['name']}: {source}"
        )
        url = (
            f"https://static.crates.io/crates/{package['name']}/"
            f"{package['name']}-{package['version']}.crate"
        )
        expected[url] = package["checksum"]
    return expected


def parse_and_validate_cargo_sources(sources_path: str) -> dict[str, str]:
    sources = json.loads(read(sources_path))
    archives: dict[str, str] = {}
    archive_dests: dict[str, str] = {}
    checksum_dests: dict[str, str] = {}
    for source in sources:
        if source.get("type") == "archive":
            url = source["url"]
            dest = source["dest"]
            sha256 = source["sha256"]
            assert re.fullmatch(r"[0-9a-f]{64}", sha256)
            assert url not in archives, f"duplicate archive {url} in {sources_path}"
            assert dest not in archive_dests, (
                f"duplicate vendor dest {dest} in {sources_path}"
            )
            archives[url] = sha256
            archive_dests[dest] = sha256
        elif (
            source.get("type") == "inline"
            and source.get("dest-filename") == ".cargo-checksum.json"
        ):
            dest = source["dest"]
            payload = json.loads(source["contents"])
            assert dest not in checksum_dests
            checksum_dests[dest] = payload["package"]

    assert archive_dests == checksum_dests, (
        f"mismatched .cargo-checksum.json entries in {sources_path}"
    )
    assert any(
        source.get("dest-filename") == "config"
        and 'replace-with = "vendored-sources"' in source.get("contents", "")
        for source in sources
    )
    return archives


def test_flatpak_cargo_sources_match_the_lockfile() -> None:
    head_expected = expected_crates_from_lockfile(read("Cargo.lock"))
    release_archives = parse_and_validate_cargo_sources(
        "packaging/flatpak/cargo-sources.json"
    )
    assert release_archives == head_expected

    flathub_archives = parse_and_validate_cargo_sources(
        "packaging/flatpak/flathub/cargo-sources.json"
    )
    flathub_manifest_path = f"packaging/flatpak/flathub/{APP_ID}.json"
    flathub_manifest = json.loads(read(flathub_manifest_path))
    flathub_conduit = find_conduit_module(flathub_manifest, flathub_manifest_path)
    archive_url = flathub_conduit["sources"][0]["url"]
    tag_match = re.search(r"/refs/tags/(v\d+\.\d+\.\d+)\.tar\.gz$", archive_url)
    assert tag_match is not None
    pinned_tag = tag_match.group(1)
    git = shutil.which("git")
    if (ROOT / ".git").exists() and git is not None:
        tag_lockfile = subprocess.run(
            [git, "-C", str(ROOT), "show", f"refs/tags/{pinned_tag}:Cargo.lock"],
            capture_output=True,
            text=True,
            check=False,
        )
        if tag_lockfile.returncode == 0:
            assert flathub_archives == expected_crates_from_lockfile(
                tag_lockfile.stdout
            ), (
                f"packaging/flatpak/flathub/cargo-sources.json does not match "
                f"{pinned_tag}:Cargo.lock"
            )


def test_unstable_http3_stack_is_not_locked_or_vendored() -> None:
    manifest = tomllib.loads(read("Cargo.toml"))
    reqwest = manifest["dependencies"]["reqwest"]
    rustls = manifest["dependencies"]["rustls"]
    assert "rustls-no-provider" in reqwest["features"]
    assert "http2" in reqwest["features"]
    assert "rustls" not in reqwest["features"]
    assert "http3" not in reqwest["features"]
    assert rustls["features"] == ["aws-lc-rs"]

    lockfile = tomllib.loads(read("Cargo.lock"))
    locked_packages = {package["name"] for package in lockfile["package"]}
    unused_http3_packages = {
        "h3",
        "h3-quinn",
        "quinn",
        "quinn-proto",
        "quinn-udp",
    }
    assert locked_packages.isdisjoint(unused_http3_packages)

    for sources_path in FLATPAK_CARGO_SOURCES_PATHS:
        sources = read(sources_path)
        assert all(
            f"/{package}/" not in sources and f"/{package}-" not in sources
            for package in unused_http3_packages
        )


def main() -> None:
    tests = [
        test_release_versions_are_synchronized,
        test_rust_toolchain_is_pinned_across_local_ci_and_release,
        test_local_meson_suite_enforces_strict_rust_lints,
        test_local_meson_suite_enforces_rust_formatting,
        test_cargo_build_depends_on_compiled_resources,
        test_release_workflow_builds_validates_and_publishes_all_assets,
        test_generated_release_pull_requests_use_verified_dispatched_ci,
        test_release_build_tests_reuse_the_release_profile,
        test_release_packages_disable_nonproduction_features,
        test_release_and_flathub_flatpak_manifests_are_hardened,
        test_flatpak_cargo_sources_match_the_lockfile,
        test_unstable_http3_stack_is_not_locked_or_vendored,
    ]
    for test in tests:
        test()
    print(f"release automation checks passed ({len(tests)} tests)")


if __name__ == "__main__":
    main()
