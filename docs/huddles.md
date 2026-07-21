# Slack huddles

Conduit detects active Slack huddles from supported conversation and user-presence data. Passive discovery appears only in the matching workspace and conversation; after the user opens or joins that huddle, its controls stay visible while they navigate. The visible desktop notification contains no participant names or call identifiers. Selecting the huddle opens a camera-off preflight and, when native joining is unavailable, hands the exact workspace and conversation to Slack over HTTPS.

## Current interoperability boundary

Slack does not publish an API for joining its own huddles. Slack huddles use Amazon Chime, so a generic WebRTC SDP/ICE exchange is not a compatible production join path by itself. Conduit therefore enables native joining only when both of these independently report a verified compatible revision:

- the isolated Slack huddle bootstrap adapter;
- an Amazon Chime signalling and media bridge.

Neither production adapter is enabled today. Conduit still provides supported huddle discovery, participant presence, notifications, preflight, and a validated **Open in Slack** fallback. When the Slack-supplied room link matches the active workspace and conversation, the fallback is the exact canonical `https://app.slack.com/huddle/<team>/<conversation>` URL. Conduit rejects mismatched or ambiguous room links and does not use a `slack://` huddle link that could route back into itself.

The optional GStreamer engine and synthetic harness are native media infrastructure and interoperability tests. They must not be described as native Slack joining.

Debian, RPM, and Flatpak release packages deliberately leave `native_media` and `screen_share` disabled until the production adapters above are available. This keeps unexercised media dependencies and capture permissions out of general packages without changing discovery, preflight, notifications, or **Open in Slack**. CI still builds and tests the opt-in stack.

## Build options

The default build includes discovery, huddle UI state, and the external fallback without compiling the media stack. Meson exposes two opt-in features:

- `native_media` builds the GStreamer `webrtcbin` engine, device discovery, media controls, and statistics.
- `screen_share` adds the XDG ScreenCast portal and PipeWire sharing path and requires `native_media`.

On Debian 13, install the native media development and runtime packages:

```sh
sudo apt install \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-libav \
  gstreamer1.0-nice \
  gstreamer1.0-pipewire \
  pipewire wireplumber \
  xdg-desktop-portal xdg-desktop-portal-gnome
```

GStreamer 1.24 or newer is required. Configure a full local build in a separate build directory:

```sh
meson setup _build-huddles \
  -Dnative_media=enabled \
  -Dscreen_share=enabled
meson compile -C _build-huddles
meson test -C _build-huddles --print-errorlogs
```

The equivalent deterministic Rust validation is:

```sh
cargo test --locked \
  --features native-media,screen-share,huddle-harness
```

The harness uses test sources and sinks and a synthetic portal. It needs no Slack credentials, camera, microphone, desktop capture, or private endpoint.

## Privacy and permissions

- Joining and capture require explicit user actions. Opening preflight does not start capture, and changing a displayed device choice does not activate that source.
- The camera and screen sharing are off by default. Reconnect never silently restores either one.
- Screen sharing follows `CreateSession`, `SelectSources`, `Start`, and `OpenPipeWireRemote`. It asks the portal only after **Share screen** is selected, requests one monitor or window, and does not persist a restore token.
- Cancelling the portal, stopping sharing, leaving, signing out, replacing the workspace session, or shutting down closes the portal session and PipeWire file descriptor.
- SDP, ICE candidates, TURN credentials, bootstrap responses, media, and raw huddle payloads are never logged or persisted. Ephemeral negotiation values are redacted and cleared during teardown.
- An opt-in native-media Flatpak would need the standard PulseAudio socket for microphone and speaker access. The general release Flatpak does not request it while native media is disabled. Portal calls use Flatpak's filtered session bus and restricted PipeWire remotes; do not add broad filesystem, session-bus, PipeWire-socket, or `--device=all` permissions.

Direct camera device discovery is suitable for host builds. A sandboxed camera must use the XDG Camera portal rather than broad device access; until that adapter is available, packaged builds should leave camera capture unavailable.

## Troubleshooting

If a native media build cannot configure, confirm that the GStreamer core, SDP, and WebRTC development metadata are installed and visible to `pkg-config`.

If the application reports missing runtime components, verify the required plugins:

```sh
for element in \
  webrtcbin webrtcdsp webrtcechoprobe \
  opusenc opusdec vp8enc vp8dec pipewiresrc
do
  gst-inspect-1.0 "$element" >/dev/null || exit 1
done
```

If screen sharing does not present a chooser, confirm that PipeWire, WirePlumber, `xdg-desktop-portal`, and the desktop-specific portal backend are running. A denied or cancelled chooser is a normal, recoverable result and must leave sharing off.

If **Open in Slack** fails, confirm that the active workspace and conversation still match the huddle. Conduit intentionally rejects links with another host, workspace, conversation, port, query, fragment, or encoded path.
