# Conduit

<p align="center">
  <img src="data/branding/conduit-logo-big.png" alt="Conduit logo" width="360">
</p>

Conduit is a lightweight GNOME desktop client for Slack written in Rust with GTK4, libadwaita, and WebKitGTK.

The current implementation is an early development slice. It includes a native UI shell, Slack OAuth with PKCE for desktop user-token authentication, secure token storage through the system keyring, and read/write Slack Web API plumbing for conversations, history, threads, search, saved items, messages, and file uploads.

## Building

Install the GNOME development stack, WebKitGTK 6.0 and D-Bus development headers, Rust, Meson, and Ninja, then build with Meson:

```sh
meson setup _build
meson compile -C _build
meson test -C _build
```

`cargo check` and `cargo test` are also supported for Rust validation, but running the app expects the compiled GResource from the Meson build or an installed build.

Packaged builds can provide a default Slack app client ID so users only need to choose **Connect Workspace**:

```sh
meson setup _build -Dslack_client_id=1234567890.1234567890123
```

For local development, you can also set `CONDUIT_SLACK_CLIENT_ID` before running `cargo` or paste the client ID into the first-run screen.

To open the workspace connection flow explicitly, start Conduit with `--connect` or `-c`. This does not remove the current stored token; the token is replaced only after a new Slack authorization succeeds.

To debug OAuth setup, add `--debug-auth` or `-d`. This prints the Slack authorization URL and non-secret OAuth milestones to stderr without logging access tokens or authorization codes.

## Slack App Setup

Create a Slack app, enable PKCE for OAuth, and add this redirect URL:

```text
http://127.0.0.1:8934/callback
```

The client uses Slack's user-token PKCE flow (`oauth.v2.user.access`) and requests user scopes. Desktop PKCE redirects cannot request bot scopes, so Conduit avoids bot-token setup for the core workspace connection.

Required user scopes:

```text
channels:read,channels:history,groups:read,groups:history,im:read,im:history,mpim:read,mpim:history,users:read,chat:write,search:read,stars:read,stars:write,reactions:read,reactions:write,files:read,files:write
```

After approval, Conduit validates the session with `auth.test` and stores the token in the system keyring. Use the sign-out button in the workspace toolbar to remove the stored token.

## Status

Implemented:

- Native login/workspace shell.
- PKCE OAuth callback flow on localhost.
- Keyring-backed token storage.
- Background Tokio runtime for Slack network work.
- Conversation list, history, thread replies, search, saved items, message posting, emoji reactions, file upload, and Socket Mode refreshes.

Next:

- Rate-limit-aware pagination.
- Presence cache.
- Flatpak dependency vendoring and Flathub-grade screenshots.
