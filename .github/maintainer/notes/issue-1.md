# ISSUE:1 — Quick-response toolbar remains open

- Status: open, actionable
- Confidence: high
- Impact: visible interaction defect; multiple toolbars obscure messages and make the hover model feel broken
- Intent: only the hovered toolbar should remain visible after pointer interaction, while keyboard users must still be able to reveal and operate actions
- Root cause: fine-pointer CSS reveals `.quick-actions` through `.message-part:focus-within`; pointer clicks retain focus after the pointer leaves
- Recommended implementation: retain hover, use keyboard-specific `:focus-visible` behavior, remove inert `.message > .quick-actions` selectors, and cover the generated CSS contract
- Risk: verify `:has(:focus-visible)` support on the project’s WebKitGTK baseline; use explicit input-modality state if compatibility is insufficient
- Public action: none taken
