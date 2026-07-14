# Conduit

<p align="center">
  <img src="data/branding/conduit-logo-big.png" alt="Conduit logo" width="420">
</p>

<p align="center">
  A focused, native Slack client for the GNOME desktop.
</p>

Conduit is a Rust, GTK4, libadwaita, and WebKitGTK desktop client that aims to make the everyday parts of Slack feel fast and at home on Linux. It combines native workspace navigation and composers with an app-generated message timeline that escapes Slack-provided content.

The app is becoming usable as a daily driver for focused messaging, but it is still a young project. Expect gaps outside the core conversation workflow and occasional changes to setup or behavior before a stable release.

Conduit is an independent project and is not affiliated with or endorsed by Slack Technologies, LLC.

## What works today

### Conversations and navigation

- Adaptive GNOME interface for channels, direct messages, and group messages.
- Complete paginated catalog of subscribed channels, DMs, and group DMs, with persisted metadata and unread state.
- Sections for Messages, Unreads, observed threads, Files, and Later.
- Fast conversation switcher with discovery of channels and people.
- GNOME Shell search-provider integration for opening cached channels and direct messages straight from the desktop overview. It reads only conversation and name metadata for the active workspace and never indexes message history.
- Transactional SQLite caching for conversations, names, histories, threads, unread state, statuses, and custom emoji, with automatic migration from the earlier JSON cache.
- Unread badges, muted and external-conversation indicators, read markers, and desktop notifications for incoming realtime events.
- Slack status emoji and hover text for people in direct messages, shown consistently in navigation, switchers, titles, and message authors.
- Multi-word, case-insensitive substring filtering with globally ranked, category-free results across conversation, forwarding, message, and emoji searches. Conversation ranking treats direct messages and people as one-person groups, while group DMs use the share of other participants whose names match; group titles omit your own name.

### Messaging

- Paged channel history and threaded replies.
- Multiline message and thread composers with persistent per-conversation drafts.
- File uploads with progress reporting, including pasting clipboard screenshots directly into either composer.
- Emoji completion after typing a colon and at least two characters, with filtered keyboard navigation in main messages and threads.
- Edited and deleted messages, Slack links and mentions, user-group mentions, common Block Kit content, code blocks, attachments, and image and video previews.
- Workspace custom emoji in messages, reactions, composer completion, and the reusable searchable emoji picker.
- Add and remove reactions, save messages for Later, copy message text or links, and forward messages.

### Search, files, and media

- Workspace message search with Slack modifiers such as `from:`, `in:`, and `has:` preserved.
- A persistent Threads inbox assembled from fetched history, opened threads, realtime replies, and Slack subscription/unread metadata.
- Relevance-ranked multi-term results while retaining Slack's own result order for close matches.
- Files and saved-item views with navigation back to their source messages.
- Slack message permalinks for the connected workspace open directly inside Conduit.
- Internal image and video viewer with galleries, zoom, fullscreen, context actions, and Save As.
- Unsupported Slack attachments download through authenticated, size-bounded local caching and open in the system's default application; old cache entries are evicted automatically.

### Sync and resilience

- Network and cache work runs away from the GTK UI thread, with short connection, request, and Socket Mode liveness deadlines.
- Optional Slack Socket Mode ingestion for message, reaction, and conversation updates.
- Realtime persistence is ordered through a bounded, session-owned queue; messages are cached for unopened conversations, and unread DMs are prioritized for background history refresh.
- Automatic Socket Mode reconnect with capped backoff.
- Scoped loading and error recovery so failures in one surface do not replace unrelated content.
- Workspace state has one authoritative owner, while the WAL-backed SQLite cache applies incremental entity updates and supports concurrent desktop search reads.
- Tokens are validated with `auth.test` and stored through the system Secret Service/keyring.

## Current limitations

- Conduit currently manages one connected workspace session at a time.
- OAuth requires your own Slack app unless a packaged build supplies a client ID.
- Socket Mode is optional and requires separate Slack app configuration and an `xapp-` token.
- Workspace search is bounded by Slack's search API and cannot guarantee arbitrary middle-of-word discovery outside the candidates Slack returns.
- Slack's public API cannot enumerate every historical subscribed thread. Conduit retains and reconciles every thread it discovers, but a fresh installation builds its thread catalog progressively as history and replies are fetched.
- Threads and Unreads reflect the conversations and activity Conduit has observed; they are not complete Slack-wide activity aggregators.
- File and workspace-search views currently load a bounded result set rather than every page.
- Rich composer formatting, autocomplete beyond emoji, message editing/deletion controls, typing indicators, live presence, avatars, calls, canvases, workflows, custom sidebar sections, and full Slack administration are not implemented.
- The Flatpak manifest is intended for development; Conduit is not yet published on Flathub and does not currently provide official binary releases.
- Signing out removes the stored credential and clears the active-workspace selection, but it does not currently purge cached workspace data or saved drafts from local storage.

## Build and run

Install Rust, Meson, Ninja, and the development packages for GTK4, libadwaita, WebKitGTK 6.0, GLib/GIO, D-Bus, gettext, and Secret Service. Package names vary by distribution.

Build and test with Meson:

```sh
meson setup _build
meson compile -C _build
meson test -C _build
```

Run the development build directly:

```sh
_build/src/conduit
```

The binary expects `conduit.gresource` beside it or in the configured package-data directory, so a Meson build or installation is recommended over invoking `cargo run` directly.

To install under a local prefix:

```sh
meson setup _build --prefix="$HOME/.local" --reconfigure
meson compile -C _build
meson install -C _build
```

Useful Rust-only checks while developing:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Connect a Slack workspace

### Recommended: OAuth with PKCE

Create a Slack app, configure user-token OAuth, and add this redirect URL. Conduit performs the authorization using PKCE:

```text
http://127.0.0.1:8934/callback
```

Configure these user scopes:

```text
channels:read,channels:history,channels:join,channels:write,
groups:read,groups:history,groups:write,
im:read,im:history,im:write,
mpim:read,mpim:history,mpim:write,
users:read,usergroups:read,emoji:read,
chat:write,search:read,
stars:read,stars:write,
reactions:read,reactions:write,
files:read,files:write
```

Provide the client ID in one of three ways:

- Enter it on Conduit's connection screen.
- Set `CONDUIT_SLACK_CLIENT_ID` before starting Conduit.
- Embed it in a packaged build:

  ```sh
  meson setup _build -Dslack_client_id=1234567890.1234567890123
  ```

Choose **Connect Workspace**, approve access in the browser, and return to Conduit. If scopes change later, sign out, update the Slack app, and reconnect so Slack issues a new grant.

Desktop PKCE uses `oauth.v2.user.access` and user scopes. Conduit does not require a client secret or bot token for its core workspace connection.

### Advanced: import a browser session

Conduit can import `xoxc-*` and `xoxd-*` browser-session credentials. Enable **Use XOXC/XOXD tokens** on the connection screen, or set:

```sh
export CONDUIT_SLACK_XOXC_TOKEN=xoxc-...
export CONDUIT_SLACK_XOXD_TOKEN=xoxd-...
export CONDUIT_SLACK_USER_AGENT="Mozilla/5.0 ..." # optional
```

The aliases `SLACK_MCP_XOXC_TOKEN`, `SLACK_MCP_XOXD_TOKEN`, and `SLACK_MCP_USER_AGENT` are also accepted.

Browser-session credentials are highly sensitive and rely on an unofficial authentication path. Keep them out of shell history, logs, commits, screenshots, and issue reports. Unset the variables after import if you do not want Conduit to import them again after sign-out.

## Optional realtime updates

Enable Socket Mode in your Slack app, create an app-level token with `connections:write`, and subscribe to the message, reaction, and conversation events you want Conduit to receive. Then start Conduit with:

```sh
export CONDUIT_SLACK_APP_TOKEN=xapp-...
```

`SLACK_APP_TOKEN` is accepted as an alias. Socket Mode starts after authentication, stops on sign-out, and reconnects automatically after transient failures. Without it, Conduit continues to work through cached state, explicit refreshes, and Slack Web API requests.

## Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| Switch conversation | `Ctrl+K` |
| Search workspace messages | `Ctrl+F` |
| Messages / Unreads / Files / Later | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` / `Ctrl+4` |
| Focus composer | `Ctrl+M` |
| Send message | `Enter` |
| Insert newline | `Shift+Enter` or `Ctrl+Enter` |
| Complete emoji | Type `:` and at least two characters, then `Enter` or `Tab` |
| Upload file | `Ctrl+O` |
| Close thread | `Ctrl+Shift+W` |
| Refresh conversations | `F5` |
| Show shortcuts | `Ctrl+?` |
| Preferences | `Ctrl+,` |
| Quit | `Ctrl+Q` |

## Command-line options

```text
-c, --connect       Open the workspace connection flow
-d, --debug         Enable UI, rendering, cache, and Slack diagnostics
    --debug-auth    Enable OAuth diagnostics only
```

Debug output can contain private workspace metadata such as channel names, user IDs, timestamps, and unread counts. It should not contain tokens or authorization codes, but always review and redact logs before sharing them.

## Local data and security

- OAuth tokens and imported browser-session credentials are stored through the system Secret Service/keyring.
- Workspace metadata, resolved names and statuses, emoji information, and message and thread history are stored in `state/state.sqlite3` below Conduit's XDG cache directory. Downloaded attachments, image/media data, and WebKit data are cached in sibling directories. None has additional application-level encryption.
- Drafts and preferences are stored through GSettings.
- **Sign Out** removes the keyring credential and deactivates the workspace for desktop search. It does not currently erase cached workspace content or drafts, and credential environment variables remain available for re-import.
- Authenticated preview, media, and attachment downloads accept only trusted Slack HTTPS URLs and enforce size bounds. Conduit also restricts message navigation to supported internal actions and HTTP(S) links and disables file-URL access and several unused WebKit capabilities. This is not a claim of a formal security audit.

Never share tokens, cookies, private messages, or unredacted debug logs. See [SECURITY.md](SECURITY.md) for vulnerability-reporting guidance.

## Troubleshooting

- If a feature reports missing permissions, sign out, update the Slack app's user scopes, and reconnect to obtain a fresh grant.
- If a development build cannot find `conduit.gresource`, run it from the Meson build tree or set `CONDUIT_RESOURCE_PATH` to the generated resource bundle.
- If credentials cannot be stored, confirm that a Secret Service-compatible keyring is installed and unlocked.
- If realtime updates are absent, verify Socket Mode, event subscriptions, and the `xapp-` token. Core Web API workflows remain available without Socket Mode.
- Use `--debug-auth` for OAuth problems and `--debug` for wider diagnostics, then redact output before sharing it.

## Project direction

The near-term goal is a dependable, keyboard-friendly client for daily channel, DM, thread, unread, reaction, search, saved-item, and file workflows. Broader Slack surface parity will follow where public APIs and a native desktop experience make it practical.

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) before starting larger work, and use the guidance in [SECURITY.md](SECURITY.md) for sensitive reports. The project is licensed under [GPL-3.0-or-later](LICENSE).
