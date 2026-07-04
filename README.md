# Conduit

<p align="center">
  <img src="data/branding/conduit-logo-big.png" alt="Conduit logo" width="360">
</p>

Conduit is a lightweight GNOME desktop client for Slack written in Rust with GTK4, libadwaita, and WebKitGTK.

The current implementation is an early development slice. It includes a native UI shell with a GTK sidebar for workspace navigation, Slack OAuth with PKCE for desktop user-token authentication, secure token storage through the system keyring, and read/write Slack Web API plumbing for conversations, history, threads, search, saved items, messages, and file uploads.

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

Advanced users can also import a Slack browser session with an `xoxc-*` token and `xoxd-*` cookie, following the [Slack MCP XOXC/XOXD setup documentation](https://github.com/korotovsky/slack-mcp-server/blob/master/docs/03-configuration-and-usage.md). On the connection screen, enable **Use XOXC/XOXD tokens**, paste both values, and connect. Conduit validates them with `auth.test`, then stores them in the keyring.

For scripted setup, Conduit also imports these values from the environment when no token is already stored:

```sh
export CONDUIT_SLACK_XOXC_TOKEN=xoxc-...
export CONDUIT_SLACK_XOXD_TOKEN=xoxd-...
# Optional, for enterprise workspaces that require a browser-like user agent:
export CONDUIT_SLACK_USER_AGENT="Mozilla/5.0 ..."
```

The Slack MCP variable names `SLACK_MCP_XOXC_TOKEN`, `SLACK_MCP_XOXD_TOKEN`, and `SLACK_MCP_USER_AGENT` are accepted as aliases. These values are browser-session credentials; keep them secret and unset them after import if you do not want Conduit to re-import them after signing out.

To open the workspace connection flow explicitly, start Conduit with `--connect` or `-c`. This does not remove the current stored token; the token is replaced only after a new Slack authorization succeeds.

To debug rendering and Slack loading, add `--debug` or `-d`. This prints message rendering, emoji, image preview, and full Slack conversation property diagnostics to stderr. Conversation diagnostics can include private workspace metadata such as channel names, user IDs, read timestamps, and unread counts. To debug only OAuth setup, add `--debug-auth`; `--debug` includes those OAuth diagnostics too. The logs do not include access tokens or authorization codes.

## Slack App Setup

Create a Slack app, enable PKCE for OAuth, and add this redirect URL:

```text
http://127.0.0.1:8934/callback
```

The client uses Slack's user-token PKCE flow (`oauth.v2.user.access`) and requests user scopes. Desktop PKCE redirects cannot request bot scopes, so Conduit avoids bot-token setup for the core workspace connection.

Required user scopes:

```text
channels:read,channels:history,channels:write,groups:read,groups:history,groups:write,im:read,im:history,im:write,mpim:read,mpim:history,mpim:write,users:read,chat:write,search:read,stars:read,stars:write,reactions:read,reactions:write,files:read,files:write
```

If you connected Conduit before new scopes were added, Slack may keep using the older grant. Use **Sign Out** in the workspace menu, confirm the app has the scopes above, and reconnect so read markers, search, saved items, reactions, and file access have the permissions they expect.

After approval, Conduit validates the session with `auth.test` and stores the token in the system keyring. Browser-session token imports use the same validation and storage path. Use **Sign Out** in the workspace menu to remove the stored token.

## Status

Implemented:

- Native login/workspace shell.
- PKCE OAuth callback flow on localhost.
- Keyring-backed token storage.
- Background Tokio runtime for Slack network work.
- Native sidebar conversation navigation with muted/external indicators, Activity, recent files, cached conversations and recent histories, paged channel and thread history, read-marker updates, search, saved items, multiline message posting, emoji reactions, edited/deleted message rendering, Block Kit action deep links, and file upload.

Next:

- Newer-message timestamp refresh and explicit read/unread actions.
- Composer formatting controls, autocomplete, emoji picker, and draft persistence.
- Multi-workspace switching, custom sections, presence, and avatars.
- Search result tabs, people/channel directories, file pagination, and richer file previews.
- Native summaries for additional Slack product references where APIs are stable.
- Slack-wide mention, reply, and reaction aggregation for Activity.
- Optional Socket Mode realtime sync.
- Presence cache.
- Flatpak dependency vendoring and Flathub-grade screenshots.
