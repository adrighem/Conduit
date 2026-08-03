# Lazy WebView and Emoji Measurements

## Environment

- Date: 2026-07-29
- OS: Debian GNU/Linux 13 (trixie), Linux 7.1.4-native-xanmod1 x86_64
- CPU: 13th Gen Intel(R) Core(TM) i7-1355U, 12 logical CPUs
- Rust: 1.95.0
- WebKitGTK: 2.52.3
- GTK: 4.22.4
- libadwaita: 1.9.2

## Credential-free document fixture

The ignored release test renders one synthetic message with 256 synthetic workspace emoji. It
does not start the application, read the local cache, or require Slack credentials.

Reproduce with:

```sh
env -i \
  PATH=/usr/local/bin:/usr/bin:/bin \
  LANG=C.UTF-8 \
  CI=true \
  cargo test --release measure_credential_free_emoji_picker_document_cost \
  -- --ignored --nocapture
```

### Before Phase 1

Commit: `f8d8007`

| Measurement | Value |
| --- | ---: |
| Initial document bytes | 748,787 |
| Eager picker choices | 2,126 |
| Rust document generation | 5,531 us |

Generation time is a single cold release-test sample and is directional. Document bytes and picker
choice count are deterministic for the fixture.

### After bounded picker shell

Implementation base: `bfe6e22`

| Measurement | Before | After | Change |
| --- | ---: | ---: | ---: |
| Initial document bytes | 748,787 | 58,762 | -92.2% |
| Eager picker choices | 2,126 | 0 | -100% |
| Rust document generation | 5,531 us | 407 us | -92.6% |

The after-state keeps only the picker shell and category tabs in the initial document. Emoji
choices are requested from the native model when the picker opens and each result page is bounded
to 64 entries. The generation time remains a single cold release-test sample; document bytes and
picker choice count remain deterministic.

## Lazy thread WebView lifecycle

Date: 2026-08-03

The production GNOME search-provider integration test opens a real Conduit window, observes the
thread renderer state, opens a thread through the application action, navigates back to the
conversation, and reopens the same thread. It uses only the synthetic test workspace and no Slack
credentials or private workspace content.

Reproduce with:

```sh
meson test -C _build --no-rebuild --print-errorlogs "GNOME search provider"
```

| Lifecycle point | Thread WebView creations | WebView retained | Thread visible |
| --- | ---: | --- | --- |
| Startup | 0 | No | No |
| First thread open | 1 | Yes | Yes |
| Return to conversation | 1 | Yes | No |
| Reopen thread | 1 | Yes | Yes |

The selected policy is lazy creation followed by retention until window disposal. Retention keeps
reopen behavior immediate and preserves the loaded renderer while removing all secondary WebView
construction from startup. Teardown remains deliberately out of scope until release-build memory
and reopen-latency measurements show that reclaiming the renderer outweighs recreation cost.

## Release lifecycle comparison

Date: 2026-08-03

Both revisions were compiled from the same worktree and Cargo target with Meson
`--buildtype=release -Dheadless_tests=disabled`. Each value is the median of five launches in the
synthetic test workspace under Xvfb using the X11 Cairo renderer. Startup latency runs from process
spawn until the application window is visible. Thread latency runs from D-Bus action dispatch until
the lifecycle probe reports the requested pane state. PSS is the sum of `Pss:` from
`/proc/<pid>/smaps_rollup` for Conduit and all descendants, with three samples at each lifecycle
point. No Slack session, cache, credentials, or private workspace content is used.

The eager baseline is `466b6f1`, with the `thread_open` lifecycle field applied only to the
disposable measurement worktree. The lazy implementation is `5899258`.

| Measurement | Eager baseline | Lazy current | Change |
| --- | ---: | ---: | ---: |
| Cold startup to visible | 328.40 ms | 326.31 ms | -0.6% |
| Startup process-tree PSS | 220,974 KiB | 175,719 KiB | -45,255 KiB (-20.5%) |
| Readable startup processes | 4 | 3 | -1 |
| First thread open | 17.17 ms | 16.78 ms | -2.3% |
| PSS after first thread open | 230,470 KiB | 227,114 KiB | -1.5% |
| Retained thread reopen | 32.29 ms | 31.97 ms | -1.0% |
| PSS after thread reopen | 232,244 KiB | 228,472 KiB | -1.6% |

The removed startup process and 45,255 KiB PSS reduction show that the secondary renderer is no
longer constructed eagerly. First-open and reopen latency remain within the measurement noise of
the eager implementation. After first open, both builds have four readable processes and similar
PSS, which supports retaining the renderer until window disposal instead of paying a recreation
cost after every close.

## Picker-open latency and scroll stability

The production picker JavaScript was exercised in WebKitGTK three times with the credential-free,
bounded picker fixture. The fixture deliberately waits 30 ms before returning the current native
query result, so the measurement covers dispatch, that fixed delay, result validation, and
materialization of the first 64 choices.

Reproduce with:

```sh
env -i \
  HOME="$HOME" \
  PATH=/usr/local/bin:/usr/bin:/bin \
  LANG=C.UTF-8 \
  GDK_DEBUG=no-portals \
  GDK_BACKEND=x11 \
  GSK_RENDERER=cairo \
  GTK_A11Y=none \
  XDG_SESSION_TYPE=x11 \
  CONDUIT_MEASURE_EMOJI_PICKER=1 \
  meson test -C _build --no-rebuild --verbose --repeat 3 "Bounded emoji picker"
```

| Measurement | Result |
| --- | ---: |
| First bounded picker open, median | 65 ms |
| Scroll delta after Escape close | 0 px |
| Scroll delta after reaction | 0 px |

The individual picker-open samples were 58 ms, 65 ms, and 71 ms. Both scroll assertions retain a
2 px tolerance for renderer rounding; all three observed runs reported exactly 0 px.
