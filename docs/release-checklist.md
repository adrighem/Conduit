# Release Checklist

## Before Tagging

- Run `cargo fmt --check`.
- Run `cargo test`.
- Run `meson setup _build --reconfigure`.
- Run `meson compile -C _build`.
- Run `meson test -C _build`.
- Launch `_build/src/conduit` and confirm the login screen renders.
- Test OAuth with a real Slack app client ID and `http://127.0.0.1:8934/callback`.
- If Socket Mode has been implemented for the release, test it with an `xapp-` token that has `connections:write`.
- Capture real screenshots before adding AppStream screenshot entries.

## Flatpak

- Vendor Cargo dependencies before Flathub submission.
- Replace the Flatpak source with a tagged commit.
- Confirm Secret Service access works in the sandbox.
- Confirm file upload works through the document portal or permitted download path.

## Slack App Notes

- Desktop PKCE OAuth uses user scopes through `oauth.v2.user.access`.
- Bot scopes are not requested by the desktop redirect flow.
- Socket Mode is planned as an optional advanced feature because it requires a separate app-level token and explicit Slack app configuration.
