# Release Checklist

## Before Tagging

- Run `cargo fmt --check`.
- Run `cargo test --locked`.
- Run `cargo clippy --locked --all-targets --features native-media,screen-share,huddle-harness -- -D warnings`.
- Run `cargo test --locked --features native-media,screen-share,huddle-harness`.
- Run `meson setup _build --reconfigure`.
- Run `meson compile -C _build`.
- Run `meson test -C _build`.
- Configure a separate build with `-Dnative_media=enabled -Dscreen_share=enabled`, then compile and test it.
- Confirm `webrtcbin`, `webrtcdsp`, `webrtcechoprobe`, Opus, VP8, and `pipewiresrc` are available in the packaged GStreamer runtime.
- Launch `_build/src/conduit` and confirm the login screen renders.
- From an installed build, confirm `slack://open` activates both a cold and running Conduit instance after the user selects its desktop handler.
- Confirm a browser's external-protocol prompt can hand a `slack://` link to Conduit while ordinary Slack HTTPS links stay in the browser.
- Test OAuth with a real Slack app client ID and `http://127.0.0.1:8934/callback`.
- If requested Slack scopes changed since the last tester build, verify the README scope list and reconnect instructions before tagging.
- If Socket Mode has been implemented for the release, test it with an `xapp-` token that has `connections:write`.
- Confirm an active huddle appears only in its matching workspace and conversation, and that ending it removes the indicator without restarting Conduit.
- Confirm huddle preflight does not start capture, defaults the camera and sharing off, and clearly shows the selected microphone, speaker, and camera.
- Confirm unsupported native joining offers **Open in Slack** for the exact team and conversation and does not loop through Conduit's `slack://` handler.
- Confirm visible huddle notification text contains no participant, channel, workspace, or call details and opens the matching conversation.
- In the synthetic harness, exercise offer/answer, ICE, mute, camera, screen sharing, reconnect, statistics, and immediate teardown.
- Cancel screen sharing at each portal step, then stop sharing and leave; every path must close the portal session and PipeWire remote without resuming camera or sharing after reconnect.
- Review huddle diagnostics and cache data for raw payloads, SDP, ICE/TURN credentials, browser-session values, device identifiers, or captured media.
- Capture real screenshots before adding AppStream screenshot entries.

## Flatpak

- Vendor Cargo dependencies before Flathub submission.
- Replace the Flatpak source with a tagged commit.
- Confirm Secret Service access works in the sandbox.
- Confirm file upload works through the document portal or permitted download path.
- Confirm a sandboxed browser can launch Conduit for `slack://` through the desktop portal after explicit handler selection.
- Build with `native_media` and `screen_share` enabled and confirm the runtime media plugins are present inside the sandbox.
- Confirm microphone and speaker access through the PulseAudio socket and screen sharing through the ScreenCast portal.
- Confirm the manifest adds no broad device, filesystem, PipeWire-socket, session-bus, or portal talk-name permission for huddles.
- Leave camera unavailable in the sandbox unless it uses the XDG Camera portal; do not add `--device=all` as a workaround.

## Slack App Notes

- Desktop PKCE OAuth uses user scopes through `oauth.v2.user.access`.
- Bot scopes are not requested by the desktop redirect flow.
- Socket Mode is planned as an optional advanced feature because it requires a separate app-level token and explicit Slack app configuration.
