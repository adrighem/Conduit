# Conduit

<p align="center">
  <img src="https://raw.githubusercontent.com/adrighem/Conduit/main/data/branding/conduit.png" alt="" width="280">
</p>

<p align="center">
  A focused, native Slack client for the GNOME desktop.
</p>

Conduit is an independent Slack client built with Rust, GTK4, libadwaita, and
WebKitGTK. It focuses on channels, direct messages, threads, search, files,
notifications, and keyboard-driven navigation while storing workspace credentials
in the system keyring.

Conduit is pre-1.0. It is becoming useful for daily messaging, but it does not cover
every Slack feature and its setup or behavior may still change. It is not affiliated
with or endorsed by Slack Technologies, LLC.

## Before you start

- Conduit is intentionally designed for one connected Slack workspace.
- Current release packages target x86_64 Debian 13, Fedora 44, and Flatpak. Other
  systems and architectures require a source build.
- Current GitHub packages do not embed a shared Slack client ID. To use the
  recommended OAuth flow, you need permission to create and install a Slack app in
  your workspace. An administrator may need to approve it.
- Signing out does not erase cached messages, downloaded media, preferences, or
  drafts. See [Local data and security](#local-data-and-security).

## Install

Download a package and `SHA256SUMS` from the
[latest GitHub release](https://github.com/adrighem/Conduit/releases). From the
download directory, verify the package that is present:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

Install it with the matching system tool:

```sh
# Debian 13 (Trixie)
sudo apt install ./conduit_VERSION-1_amd64.deb

# Fedora 44
sudo dnf install ./conduit-VERSION-1.fc44.x86_64.rpm

# x86_64 Flatpak host with the Flathub runtime remote
flatpak install --user ./conduit-VERSION-x86_64.flatpak
```

These are standalone packages, not an update repository. Install newer releases
manually. Conduit is not currently published on Flathub, and the single-file Flatpak
bundle does not receive automatic updates. See the
[release guide](https://github.com/adrighem/Conduit/blob/main/docs/releases.md) for
package verification and release-maintenance details.

Release packages keep the experimental native huddle media stack disabled. Huddle
discovery and the exact **Open in Slack** fallback remain available.

## Connect a Slack workspace

### Recommended: OAuth with PKCE

Use a dedicated Slack app for Conduit. Enabling
[PKCE](https://docs.slack.dev/authentication/using-pkce/) makes a Slack app a public
client and is not normally reversible, so do not reuse an unrelated production app.

The quickest reproducible setup is an app manifest:

1. Open the [Slack app dashboard](https://api.slack.com/apps).
2. Choose **Create New App**, then **From an app manifest**, and select the workspace
   you want Conduit to use.
3. Paste the manifest below, review the requested access, and create the app.

```json
{
  "display_information": {
    "name": "Conduit Desktop"
  },
  "oauth_config": {
    "redirect_urls": [
      "http://127.0.0.1:8934/callback"
    ],
    "scopes": {
      "user": [
        "channels:read",
        "channels:history",
        "channels:join",
        "channels:write",
        "groups:read",
        "groups:history",
        "groups:write",
        "im:read",
        "im:history",
        "im:write",
        "mpim:read",
        "mpim:history",
        "mpim:write",
        "users:read",
        "users:read.email",
        "users.profile:read",
        "users.profile:write",
        "usergroups:read",
        "emoji:read",
        "chat:write",
        "search:read",
        "stars:read",
        "stars:write",
        "reactions:read",
        "reactions:write",
        "files:read",
        "files:write"
      ]
    },
    "pkce_enabled": true
  },
  "settings": {
    "org_deploy_enabled": false,
    "socket_mode_enabled": false,
    "token_rotation_enabled": false
  }
}
```

4. In the Slack app's **Basic Information** page, copy its **Client ID**. Conduit
   does not need the client secret or a bot token.
5. Start Conduit, enter the Client ID, choose **Connect Workspace**, approve the
   user-token grant in your browser, and return to Conduit.

The manifest requests user scopes for conversation discovery and history, sending
messages and read markers, people and status data, search, stars, reactions, emoji,
and files. It requests no bot scopes. To configure an existing app manually, add the
same redirect URL, enable PKCE, and add the manifest's entries under
**OAuth & Permissions > User Token Scopes**.

If Conduit adds scopes in a later release, update the Slack app, sign out, and
reconnect so Slack can extend the grant.

### Advanced: import a browser session

Browser-session import is an unofficial compatibility path. XOXC and XOXD values
grant access to your signed-in Slack session and may stop working without notice.
Prefer OAuth. Never put these values in shell history, files, logs, screenshots,
commits, or issue reports.

Enable **Use XOXC/XOXD tokens** on Conduit's connection screen and paste values
directly into its password fields.

<a id="lookup-slack_mcp_xoxc_token"></a>
<details>
<summary>Show browser-session import steps</summary>

1. Open the target workspace in a signed-in browser tab.
2. Open that tab's developer console. Use the console's `copy()` helper so the XOXC
   value goes to the clipboard without being rendered as console output:

   ```js
   copy(
     JSON.parse(localStorage.localConfig_v2)
       .teams[document.location.pathname.match(/^\/client\/([A-Z0-9]+)/)[1]]
       .token
   );
   ```

   If the browser blocks console pasting, do not disable its self-XSS protection.
   Type the command or use the browser's storage inspector instead.
3. Paste the clipboard directly into Conduit's **XOXC token** field.
4. In the browser's Storage or Application tools, open Cookies, select the `d`
   cookie, copy its value, and paste it directly into Conduit's **XOXD cookie**
   field.
5. Enterprise Slack may also require the exact User-Agent from the same browser.
   Copy it without rendering the value:

   ```js
   copy(navigator.userAgent);
   ```

   Paste it into **Browser User-Agent (Enterprise)**.
6. Choose **Import Browser Session**, then overwrite or clear the clipboard values.

</details>

Conduit stores an imported session in the system keyring. It also uses Slack Web's
browser WebSocket and private unread bootstrap flow for that session. Slack does not
document either interface for third-party clients, so Conduit validates responses
defensively and falls back to normal Web API refreshes when needed.

## Highlights

- Native, adaptive GNOME navigation for channels, direct messages, group messages,
  threads, Priority, Unreads, Files, and a legacy-starred-message Later view.
- A complete cached catalog of subscribed conversations, a fast conversation
  switcher, and GNOME Shell search that indexes conversation metadata but not message
  history.
- Paged history and threads, persistent drafts, people and emoji completion, file
  uploads, clipboard-image uploads, reactions, legacy message stars, forwarding,
  and read markers.
- Sanitized rendering for Slack links and mentions, edited and deleted message
  states, common Block Kit and bot attachments, code blocks, media previews, and
  custom emoji.
- Workspace message search, source-message navigation, Slack permalink handling,
  and an internal image and video viewer.
- Status viewing and editing, person profile actions, relevant desktop
  notifications, and optional realtime updates.
- Transactional SQLite caching for responsive startup and offline context, with
  workspace credentials kept separately in the system keyring.

See the
[changelog](https://github.com/adrighem/Conduit/blob/main/CHANGELOG.md) for detailed
release changes.

## Known limitations

- One connected Slack workspace is an intentional product boundary.
- Threads and Unreads grow from history and activity Conduit has observed. A fresh
  installation cannot reconstruct every historical Slack thread or activity item.
- File and workspace-search views currently use bounded result sets.
- The view labeled **Later** uses Slack's frozen legacy stars APIs. It does not
  synchronize with items added to or removed from Slack's current Later feature.
- Priority projects Slack conversation stars. Slack's separate paid-plan VIP
  preference is not available through the public API.
- Conduit renders edited and deleted message states but does not yet provide message
  editing or deletion controls.
- Rich composer formatting, typing indicators, general live presence, sidebar
  avatars, canvases, workflows, custom sidebar sections, and full Slack
  administration are not implemented.
- Native production huddle joining is not implemented. Conduit discovers supported
  huddles and opens the exact huddle in Slack. See
  [Slack huddles](https://github.com/adrighem/Conduit/blob/main/docs/huddles.md).

## Optional realtime updates

OAuth workspaces work without Socket Mode through cached state, manual refresh, and
direct Web API calls. To add live message, reaction, profile, huddle, and conversation
updates:

1. In the same Slack app, open **Socket Mode** and enable it.
2. Under **Basic Information > App-Level Tokens**, generate a token with the
   `connections:write` scope.
3. Under **Event Subscriptions > Subscribe to events on behalf of users**, add the
   events Conduit currently consumes:

   ```text
   message.channels, message.groups, message.im, message.mpim
   reaction_added, reaction_removed
   user_change, user_huddle_changed
   channel_archive, channel_created, channel_deleted, channel_left
   channel_rename, channel_unarchive
   group_archive, group_joined, group_left, group_rename, group_unarchive
   im_created, member_joined_channel, member_left_channel, mpim_open
   ```

4. Paste the app-level token into **Preferences > Realtime updates** and restart
   Conduit. Tokens entered there are stored in the system keyring.

Browser-session workspaces use their browser WebSocket automatically and do not need
an app-level token.

When realtime is online, **Preferences > Notifications** controls direct-message,
mention/name, thread-reply, and keyword triggers. Configured name and keyword lists
start empty. See
[Attention and notifications](https://github.com/adrighem/Conduit/blob/main/docs/attention-and-notifications.md)
for the full policy.

## Local data and security

- OAuth and imported browser-session credentials, plus Socket Mode tokens entered in
  Preferences, are stored through the system Secret Service/keyring.
- Workspace metadata, names, statuses, emoji, and message and thread history are
  stored in `state/state.sqlite3` below Conduit's XDG cache directory. WebKit,
  downloaded image, media, and attachment data use sibling cache directories.
- Inline image and video previews are stored as MIME-checked raw files in
  workspace-scoped cache directories. Each preview is limited to 8 MiB for images
  or 16 MiB for videos; the preview cache is limited to 512 MiB and 16,384 files,
  and entries older than 30 days are removed. The active preview registry is
  limited to 64 MiB of addressable content and 2,048 entries, with bounded request
  state and no second in-memory payload copy.
- Clipboard images are temporarily encoded as PNG files in the cache's
  `upload-staging` directory. They are normally removed when the upload task ends;
  leftovers from a crash are cleared at the next startup.
- Drafts and preferences are stored through GSettings. Cache and settings data have
  no additional application-level encryption.
- **Sign Out** attempts to remove the saved user-session credential and active
  workspace selection. It reports keyring deletion failures but still completes. It
  does not remove the separately saved Socket Mode app token, cached workspace data,
  or drafts.
- Huddle media, portal sessions, SDP, ICE candidates, and TURN credentials are
  ephemeral and are not written to Conduit's cache or settings.

General `--debug` output can contain private workspace metadata such as channel names,
user IDs, timestamps, and unread counts. `--debug-auth` omits tokens, authorization
codes, OAuth state, and PKCE values, but can include client, scope, workspace, and
user identifiers. Review and redact every diagnostic before sharing it.

Never share tokens, cookies, private messages, or unredacted diagnostics. Use
[private vulnerability reporting](https://github.com/adrighem/Conduit/security/policy)
for security issues.

## Reference

### Open Slack links with Conduit

An installed Conduit desktop entry advertises Slack's `slack://` scheme but does not
change your current default handler. Inspect the current handler before selecting
Conduit:

```sh
xdg-mime query default x-scheme-handler/slack
xdg-mime default eu.vanadrighem.conduit.desktop x-scheme-handler/slack
```

Save the first command's output. To restore it later, substitute that value here:

```sh
xdg-mime default PREVIOUS_DESKTOP_ID x-scheme-handler/slack
```

Conduit accepts `open`, `channel`, `user`, `file`, `share-file`, and `app` actions.
Channels, direct messages, and files use native surfaces. App actions use Slack's
official HTTPS fallback and do not preserve a requested app tab. `share-file` opens
the file but does not share it. Unknown actions present Conduit without target
navigation.

Workspace-scoped links must match the connected workspace. Ordinary Slack HTTPS
links remain in the browser unless Slack or the browser hands off a `slack://` URI.

### Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| Switch conversation | `Ctrl+K` |
| Search workspace messages | `Ctrl+F` |
| Messages / Unreads / Files / Later (legacy stars) | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` / `Ctrl+4` |
| Focus composer | `Ctrl+M` |
| Upload file | `Ctrl+O` |
| Close thread | `Ctrl+Shift+W` |
| Show the complete shortcut list | `Ctrl+?` |

In a composer, `Enter` sends when no completion menu is open. `Shift+Enter` inserts
a newline; `Ctrl+Enter` is also accepted as a newline alias. When completion is open,
`Enter` or `Tab` accepts the selected person or emoji.

### Command-line options

```text
Usage: conduit [OPTIONS] [slack://URI ...]

-c, --connect       Disconnect the active runtime session and open the connection flow
-d, --debug         Enable general diagnostics, including OAuth stages
    --debug-auth    Enable OAuth diagnostics only
```

At most 16 positional Slack URIs are handled per invocation.

## Troubleshooting and support

- For an OAuth workspace with a newly required user scope, update the Slack app,
  sign out, and reconnect. Conversation-specific failures can instead be caused by
  membership or workspace policy.
- If a development build cannot find `conduit.gresource`, run
  the Meson-produced `_build/src/conduit` or set `CONDUIT_RESOURCE_PATH` to
  `_build/src/conduit.gresource`. If GSettings reports a missing schema, compile and
  select it with the two schema commands shown under **Build from source**.
- If credentials cannot be stored, confirm that a Secret Service-compatible keyring
  is installed and unlocked.
- If OAuth realtime is absent, verify Socket Mode, the user-event subscriptions, and
  the app-level token under **Preferences > Realtime updates**.
- For a browser session, re-import XOXC and XOXD from the same signed-in browser.
  Enterprise Slack may also require that browser's exact User-Agent. Use OAuth if
  browser TLS fingerprint requirements still block the session.
- If a `slack://` link opens another client, check the current handler with
  `xdg-mime query default x-scheme-handler/slack` and select Conduit as shown above.
- If native huddle joining is unavailable, use **Open in Slack**. This is the
  expected fallback.

For other problems, search or open a
[GitHub issue](https://github.com/adrighem/Conduit/issues) with the Conduit version,
operating system, and desktop environment. Do not attach private workspace content
or unredacted diagnostics. Report sensitive problems through
[SECURITY.md](https://github.com/adrighem/Conduit/blob/main/SECURITY.md).

## Build from source

The default build requires:

- Rust 1.88 or newer. The repository pins the reviewed Rust, rustfmt, and Clippy
  version in `rust-toolchain.toml`, currently 1.97.1.
- Meson 1.0 or newer, Ninja, CMake, a C compiler, pkg-config, and gettext.
- Development packages for GTK4, libadwaita, WebKitGTK 6.0, GdkPixbuf, GLib/GIO,
  and D-Bus.
- Python 3 with PyGObject and GdkPixbuf introspection for the headless UI tests.
- A running Secret Service-compatible keyring when connecting a workspace.

Exact package names are recorded in the
[Debian control file](https://github.com/adrighem/Conduit/blob/main/packaging/debian/control)
and
[Fedora RPM spec](https://github.com/adrighem/Conduit/blob/main/packaging/rpm/conduit.spec).

Build, test, and run:

```sh
meson setup _build
meson compile -C _build
meson test -C _build --print-errorlogs
glib-compile-schemas --strict --targetdir=_build/data data
GSETTINGS_SCHEMA_DIR="$PWD/_build/data" _build/src/conduit
```

Use the Meson-produced executable. It expects `conduit.gresource` beside the binary
or in the configured package-data directory, plus the compiled GSettings schema
selected above, so `cargo run` alone is not enough.

To install under your user prefix:

```sh
meson setup _build --prefix="$HOME/.local" --reconfigure
meson compile -C _build
meson install -C _build
```

Reinstalling from the same build directory upgrades the source installation and
removes known obsolete branding files. Fully quit and reopen Conduit afterward. If
GNOME Shell retains an old icon, sign out and back in once.

To remove the files recorded by that build directory:

```sh
ninja -C _build uninstall
```

Use the same build directory and permissions used for installation. Native huddle
media and screen sharing are optional development features; their dependencies and
test matrix are documented in
[Slack huddles](https://github.com/adrighem/Conduit/blob/main/docs/huddles.md).
See
[CONTRIBUTING.md](https://github.com/adrighem/Conduit/blob/main/CONTRIBUTING.md) for
the complete contributor checks.

## Contributing and license

The near-term goal is a dependable, keyboard-friendly client for everyday
conversation, thread, unread, reaction, search, legacy-starred-message, and file
workflows. Contributions are welcome. Read
[CONTRIBUTING.md](https://github.com/adrighem/Conduit/blob/main/CONTRIBUTING.md)
before starting larger work.

Conduit is licensed under the
[GNU General Public License v3.0 or later](https://github.com/adrighem/Conduit/blob/main/LICENSE).
