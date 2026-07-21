# Releases

Conduit uses Release Please to turn conventional commits on `main` into reviewed release pull requests. Merging a release pull request creates a draft GitHub Release, then builds and validates every package before publishing the release.

## Cutting a release

1. Merge changes using conventional commit titles such as `fix: ...`, `feat: ...`, or a breaking `feat!: ...`.
2. Review the automated `chore(main): release ...` pull request, including `CHANGELOG.md` and the synchronized Cargo, Meson, and AppStream versions.
3. Merge that pull request when the release is ready.
4. Wait for the Release workflow to build and install-validate all three package formats. It attaches the packages and `SHA256SUMS`, then changes the GitHub Release from draft to published.

The first run creates `v0.1.0`. Thereafter fixes increment the patch version, features increment the minor version, and breaking changes increment the major version. Tags use `v<version>` without a component prefix.

The workflow uses the repository `GITHUB_TOKEN` by default. Repository Actions settings must allow GitHub Actions to create pull requests. An optional fine-grained `RELEASE_PLEASE_TOKEN` secret with Contents, Issues, and Pull requests read/write permissions lets Release Please pull requests trigger normal CI runs; the workflow automatically prefers it when present.

## Release assets

The first packaging tier is x86_64 only:

- `conduit_<version>-1_amd64.deb`, built and installed on Debian 13 (Trixie).
- `conduit-<version>-1.fc44.x86_64.rpm`, built and installed on Fedora 44.
- `conduit-<version>-x86_64.flatpak`, built offline against the GNOME 50 runtime.
- `SHA256SUMS`, covering all three packages.

General release packages intentionally disable the optional `native-media` and `screen-share` features while production Slack huddle joining is unavailable. Huddle discovery, preflight, and the exact **Open in Slack** fallback remain available. CI continues to compile and test the optional media stack and synthetic harness, but those experiments do not add media dependencies or capture permissions to release packages.

Clean validation containers install each native package, check dynamic libraries and RPATH, and validate desktop, AppStream, and GSettings metadata. A separate privileged Flatpak validation job installs the generated bundle, verifies its ref, runtime, commit, installed files, release metadata, and permissions, and executes a command inside the sandbox. Asset publication depends on all three validation jobs.

Update the pinned Debian/Fedora targets when either distribution leaves support. Update `RUST_VERSION` when the minimum Rust version in `Cargo.toml` or locked dependencies requires it.

## Updating Flatpak dependencies

The release Flatpak manifest is [packaging/flatpak/eu.vanadrighem.conduit.json](../packaging/flatpak/eu.vanadrighem.conduit.json). It is separate from the root development manifest and builds the exact checked-out release commit without network access.

Whenever `Cargo.lock` changes, regenerate `packaging/flatpak/cargo-sources.json` with the official Flatpak Cargo generator:

```sh
python3 flatpak-cargo-generator.py Cargo.lock \
  --output packaging/flatpak/cargo-sources.json
```

The generator is maintained in [flatpak-builder-tools](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo). Commit the lockfile and generated source manifest together.

## Flatpak publication

The GitHub Release `.flatpak` is a directly installable bundle, not an update repository. Flatpak repositories or Flathub are needed for automatic updates.

Flathub onboarding is deliberately not automated here. It requires a separate manifest pinned to an immutable tag and commit, screenshots and complete AppStream metadata, domain verification, a human submission and review, and ongoing updates in the generated Flathub repository.

Flathub's current requirements also prohibit AI-generated or AI-assisted application and submission content except by case-by-case exception. Conduit contains AI-assisted work, so a maintainer must first obtain an explicit Flathub exception and personally own any submission and review discussion. Without that exception, the supported Flatpak route remains GitHub Release bundles; a signed self-hosted Flatpak repository is a possible future alternative.
