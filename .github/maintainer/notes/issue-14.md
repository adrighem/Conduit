# ISSUE:14 — Dependencies and native-media packaging

- Status: open; implementation is on `main` at `af7ed57` and remote CI is green, while real release-package validation remains pending
- Confidence: high
- Impact: removes unused supply-chain/build surface and decides whether general packages should enable unverified native huddle capabilities
- Intent: remove unused direct dependencies, measure the graph/build impact, and align Cargo, Meson, CI, and all package formats on one documented feature policy
- Relationship: the code/configuration blocker for PR:8 is resolved; the remaining evidence comes from the first real package workflow after merge
- Resolution: removed three unused direct dependencies; regenerated the lockfile and Flatpak sources; disabled native media and screen sharing in Debian, RPM, and release Flatpak definitions; removed package-only media dependencies and capture permission; retained CI's opt-in feature matrix and the external Slack fallback
- Validation: local default Cargo/Meson checks pass; main and PR CI pass strict Clippy, the optional native-media lint/test matrix, and both Meson configurations
- Risks: the real package jobs have not run with this change, and the first-release manual smoke checklist remains incomplete
- Next step: complete the manual first-release checklist, merge PR:8 only with explicit approval, monitor every package job, then close ISSUE:14 only after the real artifacts validate
- Public action: implementation pushed with `Refs #14`; no issue comment, label, or closure
