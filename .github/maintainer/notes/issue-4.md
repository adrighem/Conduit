# ISSUE:4 — Show filtered emoji picker when typing :xx

- Status: reopened regression fixed locally; closure-ready after remote CI
- Confidence: high
- Root cause: the composer popover used autohide, which took focus as it opened and immediately triggered the focus-loss dismissal path
- Fixed: non-autohiding composer-owned popover, all valid shortcode characters after the two-character threshold, and real workspace custom emoji previews
- Validation: pure token/catalog tests plus a repeated Xvfb GTK test covering open, navigation, acceptance, Escape dismissal, and both composers
- Public action: none taken
