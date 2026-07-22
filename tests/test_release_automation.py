#!/usr/bin/env python3

import json
import re
import sys
import tomllib
from pathlib import Path
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
    metainfo = ElementTree.parse(
        ROOT / "data" / f"{APP_ID}.metainfo.xml.in"
    ).getroot()

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
    assert "x-release-please-version-date" in read(
        f"data/{APP_ID}.metainfo.xml.in"
    )

    meson_version = re.search(r"version:\s*'([^']+)'", meson)
    appstream_release = metainfo.find("./releases/release")
    assert meson_version is not None
    assert appstream_release is not None
    assert cargo["package"]["version"] == meson_version.group(1)
    assert cargo["package"]["version"] == appstream_release.attrib["version"]
    assert manifest == {".": cargo["package"]["version"]}


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
    assert ignored_paths == RELEASE_PR_FILES
    workflow_dispatch = workflow_event(ci_workflow, "workflow_dispatch")
    assert "      commit_sha:" in workflow_dispatch
    assert "        required: true" in workflow_dispatch
    assert "        type: string" in workflow_dispatch

    ci_job = workflow_jobs(ci_workflow)["build"]
    checkout = step_containing(workflow_steps(ci_job), CHECKOUT_ACTION)
    assert "ref: ${{ inputs.commit_sha || github.sha }}" in checkout
    assert "persist-credentials: false" in checkout

    release_workflow = read(".github/workflows/release.yml")
    release_jobs = workflow_jobs(release_workflow)
    release_job = release_jobs["release-please"]
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


def test_release_flatpak_uses_current_checkout_without_debug_logging() -> None:
    manifest = json.loads(read(f"packaging/flatpak/{APP_ID}.json"))

    assert manifest["id"] == APP_ID
    assert manifest["runtime"] == "org.gnome.Platform"
    assert manifest["runtime-version"] == "50"
    assert manifest["command"] == "conduit"
    assert "--socket=pulseaudio" not in manifest["finish-args"]
    assert not any(
        argument == "--env=RUST_LOG"
        or argument.startswith("--env=RUST_LOG=")
        for argument in manifest["finish-args"]
    )

    conduit = next(
        module for module in manifest["modules"] if module["name"] == "conduit"
    )
    assert conduit["sources"][0] == {"type": "dir", "path": "../.."}
    assert "cargo-sources.json" in conduit["sources"]
    assert any(
        source.get("type") == "shell"
        and "cp -vf cargo/config .cargo/config.toml" in source.get("commands", [])
        for source in conduit["sources"]
        if isinstance(source, dict)
    )
    assert "--libdir=lib" in conduit["config-opts"]
    assert_native_media_disabled(conduit["config-opts"], "Flatpak package")
    assert_headless_tests_disabled(conduit["config-opts"], "Flatpak package")
    for scope in (manifest, *manifest["modules"]):
        environment = scope.get("build-options", {}).get("env", {})
        assert "RUST_LOG" not in environment


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
        test_release_workflow_builds_validates_and_publishes_all_assets,
        test_generated_release_pull_requests_use_verified_dispatched_ci,
        test_release_build_tests_reuse_the_release_profile,
        test_release_packages_disable_nonproduction_features,
        test_release_flatpak_uses_current_checkout_without_debug_logging,
        test_flatpak_cargo_sources_match_the_lockfile,
    ]
    for test in tests:
        test()
    print(f"release automation checks passed ({len(tests)} tests)")


if __name__ == "__main__":
    main()
