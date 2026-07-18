# Native Slack Huddles

## Summary

Add GNOME-native Slack huddle support to Conduit: discover active huddles from Slack's supported metadata and events, present trustworthy join and in-call controls, provide a native GStreamer/PipeWire WebRTC media engine for audio, camera, and portal-based screen sharing, and isolate Slack's undocumented first-party join protocol behind a capability-checked signalling adapter. When native Slack bootstrap is unavailable or changes, keep discovery useful and offer an explicit external Slack fallback instead of failing silently or misusing the Calls API.

## Requirements

1. Model huddle identity, channel, participants, presence, media state, device selections, statistics, and failures independently from GTK widgets and transport details.
2. Derive active-huddle discovery from supported Slack conversation metadata and `user_huddle_changed` events, correlating users through the huddle call ID without logging raw event payloads.
3. Add a deterministic coordinator state machine covering idle, discovered, preflight, joining, connected, reconnecting, leaving, failed, and externally handed-off states.
4. Extend Conduit's runtime command/event boundary for discovery, join/leave, mute, camera, device selection, screen sharing, roster changes, statistics, and failures without blocking GTK's main thread.
5. Implement a native media controller around GStreamer and `webrtcbin`, with Opus audio, camera/video tracks, negotiated ICE/SDP handling, bounded latency, explicit teardown, media-device discovery, and redacted statistics.
6. Implement screen sharing only through the XDG ScreenCast portal and PipeWire. Portal permission must be requested only after an explicit user action, and the portal session must close immediately when sharing or the huddle ends.
7. Keep Slack-specific join/bootstrap and signalling behind a narrow adapter. Undocumented browser-session behavior must be capability checked, feature isolated, comprehensively redacted, and replaceable without changing the coordinator or media engine.
8. Provide a deterministic synthetic signalling/media harness so the complete join, negotiation, media-control, reconnect, and teardown flow can be tested without Slack credentials or private endpoints.
9. Add a native libadwaita huddle surface with active-huddle discovery, a camera-off-by-default preflight, mic/camera/share indicators, mute/camera/share/leave controls, concise failures, and accessible labels.
10. Provide GNOME notifications for actionable huddle state changes without exposing private participant or signalling data on the lock screen.
11. If Slack's native bootstrap is not available, offer an explicit Open in Slack action scoped to the correct team and channel. Never claim that Slack's public Calls API joins first-party huddles.
12. Update Debian/Flatpak dependencies, build documentation, privacy guidance, release checks, and automated validation for the new media and portal stack.

## Acceptance Criteria

- Pure Rust tests cover huddle metadata parsing, participant correlation, coordinator transitions, privacy invariants, runtime descriptors, signalling capability decisions, and fallback construction.
- Socket Mode tests cover valid, ended, malformed, and unrelated `user_huddle_changed` events.
- An active huddle appears only for its matching conversation/workspace and updates without restarting Conduit.
- Preflight defaults the camera off, makes the selected microphone/speaker/camera clear, and does not activate capture devices.
- A synthetic end-to-end session exercises SDP/ICE exchange, audio/video graph construction, mute/camera changes, reconnect, statistics, and immediate leave teardown.
- Screen sharing follows CreateSession, SelectSources, Start, and OpenPipeWireRemote through the portal after a user click; cancellation and portal closure are safe and leave no active share.
- Native Slack join is attempted only when the signalling adapter reports a verified compatible capability. Otherwise the user receives a clear external-open fallback.
- No tokens, cookies, SDP, ICE credentials, TURN credentials, raw private payloads, or device-capture data are written to logs or persistent storage.
- The Flatpak manifest grants only the PipeWire/portal access needed for user-authorized media and does not add broad filesystem or session-bus permissions.
- Formatting, compilation, Rust tests, Meson tests, desktop/AppStream/schema validation, and available strict lint/coverage checks pass.

## Security and UX Constraints

- Joining, microphone use, camera use, and screen sharing always follow explicit user intent.
- Camera and screen sharing are off by default.
- Mic, camera, and sharing state remain visibly persistent while active.
- Leaving, signing out, switching sessions, application shutdown, and unrecoverable errors immediately tear down media and ephemeral signalling state.
- Browser-session credentials stay in the existing keyring-backed flow and never move into huddle state, diagnostics, fixtures, or cache files.
- Unsupported or changed private Slack protocol behavior degrades to external handoff; it must not trigger repeated hidden retries or capture devices in the background.

## Out of Scope

- Claiming a stable public Slack API exists for first-party huddle join/start when Slack does not document one.
- Reusing Slack's Calls API as if it controlled Slack huddles.
- Recording calls or media, background capture, transcription, clips, effects, reactions, huddle threads, or MPRIS controls.
- Chromium automation, virtual-device browser bots, compositor-specific screen capture, or a bundled conferencing server.
- Starting a new Slack huddle through an unverified private endpoint.
