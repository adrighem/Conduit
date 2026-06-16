# Conduit

Conduit is a lightweight GNOME desktop client for Slack written in Rust with GTK4 and libadwaita.

The current implementation is an early development slice. It includes a native UI shell, Slack OAuth with PKCE for desktop user-token authentication, secure token storage through the system keyring, and read/write Slack Web API plumbing for conversations, history, threads, search, saved items, messages, and file uploads.

## Building

Install the GNOME development stack, Rust, Meson, and Ninja, then build with Meson:

```sh
meson setup _build
meson compile -C _build
meson test -C _build
```

`cargo check` is also supported for Rust validation, but running the app expects the compiled GResource from the Meson build or an installed build.

## Slack App Setup

Create a Slack app and enable PKCE for desktop OAuth. Use this redirect URL:

```text
http://127.0.0.1:8934/callback
```

The client currently uses Slack's user-token PKCE flow (`oauth.v2.user.access`) and requests user scopes. Slack desktop PKCE redirects cannot request bot scopes. Socket Mode requires a separate app-level token and is intentionally left for a later opt-in slice.

## Status

Implemented:

- Native login/workspace shell.
- PKCE OAuth callback flow on localhost.
- Keyring-backed token storage.
- Background Tokio runtime for Slack network work.
- Conversation list, history, thread replies, search, saved item, message post, and file upload service methods.

Next:

- Rich Slack mrkdwn and Block Kit rendering.
- User display-name and presence cache.
- Rate-limit-aware pagination.
- Optional Socket Mode realtime updates.
- Flatpak dependency vendoring and Flathub-grade screenshots.
