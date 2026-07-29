# Release 0.3.0

- Status: preparation in progress; do not merge until the live manual checks are confirmed
- Candidate: PR:18 at generated head `332addb`
- Confidence: high in source, metadata, automation, and package gates; live desktop and Slack behavior still requires maintainer confirmation
- Provenance and risk: same-repository GitHub-signed Release Please commit; six expected text files only; no source, workflow, installer, permission, binary, credential, or dependency-graph changes

## Completed automated gates

- Exact-head CI `30450146868`: formatting, workflow lint, default and optional strict Clippy, and both Meson build/test configurations pass
- Exact-head CodeQL `30448699855`: Actions, Rust, JavaScript/TypeScript, and Python pass
- Release workflow structure: package publication waits for clean Debian, RPM, and Flatpak build/install validators and checksum generation
- ISSUE:14: controlled dependency counts and six offline clean-build samples are recorded in `docs/dependency-audit.md`
- Attention release measurements: three sanitized exact-head runs pass every semantic assertion; ranges are recorded in `docs/attention-and-notifications.md`
- Release package policy: Debian, RPM, and Flatpak explicitly disable native media, screen sharing, and the headless harness
- Flatpak policy: no PulseAudio, broad device, huddle portal, or new filesystem permission is requested

## Required maintainer confirmation

- Installed login screen and branding render correctly
- Cold and running `slack://` activation work through the selected desktop and browser handler
- PKCE reconnect grants `users.profile:write`, and status editing works with the new grant
- Socket Mode and imported browser-session realtime transports connect and reconnect
- Priority stars, DM Profile, person completion, notification filters, unread behavior, replay deduplication, and main/thread sent-message animation behave correctly
- Live huddle discovery, preflight, exact Open in Slack fallback, and notification redaction/navigation behave correctly
- Flatpak Secret Service, file upload, and desktop URI portal paths work
- Attention trace, general debug output, huddle diagnostics, and caches satisfy the documented privacy boundaries

## Conditional checks

- Flathub source replacement is not applicable to GitHub Release bundles
- Screenshots are not applicable because no AppStream screenshot entries are being added
- Production native Slack joining remains unavailable and release packages disable it; synthetic offer/answer, ICE, portal cleanup, and teardown remain covered by the optional harness

## Remaining release work

- Correct the two omitted user-facing changelog entries on PR:18
- Replace PR:18's generated ISSUE:14 closing link with a non-closing reference
- Revalidate the corrected exact PR head
- Merge and monitor package publication only after the required maintainer confirmation
