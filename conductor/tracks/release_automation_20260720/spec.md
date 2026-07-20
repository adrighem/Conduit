# Automated releases and Linux packages

## Summary

Use Release Please to maintain release pull requests, versions, changelogs, tags, and GitHub releases. A newly created release must build installable native packages against the current supported distribution targets and an installable Flatpak bundle.

## Requirements

- Run Release Please on pushes to `main` using conventional commits.
- Keep the Cargo, Cargo lockfile, Meson, AppStream, changelog, and release manifest versions synchronized.
- Use `v<semver>` Git tags and GitHub Releases.
- Build an amd64 `.deb` on Debian 13 (Trixie), the current Debian stable release at implementation time.
- Build an x86_64 `.rpm` on Fedora 44, the current Fedora release selected for packaging at implementation time.
- Stage the complete Meson installation, including desktop integration, schemas, icons, translations, D-Bus service, search provider, resources, and binary.
- Declare native runtime dependencies and validate the package metadata and installed payload in CI.
- Build an x86_64 `.flatpak` bundle from the exact release commit and attach it to the GitHub Release.
- Attach SHA-256 checksums for all release assets.
- Document how releases are cut, supported architectures, installation, and the distinction between a downloadable Flatpak bundle and publication through Flathub.
- Preserve the developer Flatpak manifest's existing local modifications while release automation is implemented.

## Acceptance criteria

- Merging a Release Please PR creates a GitHub Release and gates all packaging jobs on the action's `release_created` output.
- The release contains `.deb`, `.rpm`, `.flatpak`, and `SHA256SUMS` assets named with the released version and architecture.
- Package validation fails when the version is invalid, required installed files are absent, or metadata does not match the release.
- The release workflow can also be inspected safely through automated repository tests without creating a release.
- Documentation explains that Flathub onboarding and publication are external to this repository's GitHub Release workflow.

## Out of scope

- Publishing to the official Debian or Fedora repositories.
- Package signing, repository hosting, delta updates, or non-x86_64 native packages in the first iteration.
- Automatically opening or composing a Flathub submission.
