# Contributing to Conduit

Conduit is an early GNOME desktop client for Slack written in Rust with GTK4, libadwaita, and WebKitGTK. Contributions should keep the app stable, native-feeling, and easy to maintain.

## Before You Start

- For larger changes, open or discuss an issue first so the scope is clear.
- Do not post or commit Slack access tokens, OAuth authorization codes, cookies, workspace secrets, or private message contents.
- Keep changes focused. Avoid unrelated refactors in feature or bug-fix patches.
- External pull requests are welcome as design and implementation input, but maintainers may rework changes before merging while the project is still early.

## Development Setup

Install the GNOME development stack, WebKitGTK 6.0, D-Bus development headers, Rust, Meson, and Ninja.

Build and test with:

```sh
meson setup _build
meson compile -C _build
meson test -C _build
```

Rust-only checks are also useful while iterating:

```sh
cargo fmt --check
cargo test
```

Running the app expects the compiled GResource from the Meson build or an installed build.

## Slack App Setup

For local Slack testing, create a Slack app as described in `README.md`. Use the localhost redirect URL documented there and keep client IDs, tokens, and authorization codes out of commits and issue comments.

When changing Slack API behavior, document any new scopes, redirect behavior, token-storage changes, or app setup requirements.

## UI Guidelines

- Keep navigation, sidebars, dialogs, setup screens, and controls native GTK4/libadwaita.
- Use WebKit only for sanitized message rendering where the app already uses it.
- Put grouping, sorting, parsing, and other behavioral logic in testable Rust modules when possible.
- Match existing UI conventions before adding new patterns.

## Pull Requests

Before opening a pull request, run:

```sh
cargo fmt --check
cargo test
meson compile -C _build
meson test -C _build
```

In the pull request description, include the problem being solved, the testing performed, and screenshots for visible UI changes.
