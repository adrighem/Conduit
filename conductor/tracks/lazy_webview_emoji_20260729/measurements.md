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
