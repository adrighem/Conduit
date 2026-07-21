# ISSUE:14 — Dependencies and native-media packaging

- Status: open; implementation and real release-package validation are complete, while dependency-count and clean-build timing evidence remains outstanding
- Confidence: high
- Impact: removes unused supply-chain/build surface and keeps unsupported native huddle capabilities out of general packages
- Intent: remove unused direct dependencies, measure the graph/build impact, and align Cargo, Meson, CI, and all package formats on one documented feature policy
- Relationship: PR:8 is merged and `v0.1.0` is public; ISSUE:14 is no longer a first-release blocker
- Resolution: removed three unused direct dependencies; regenerated the lockfile and Flatpak sources; disabled native media and screen sharing in Debian, RPM, and release Flatpak definitions; removed package-only media dependencies and capture permission; retained CI's opt-in feature matrix and the external Slack fallback
- Validation: final CI passes default and opt-in native-media configurations; Release run `29835203473` built, installed, and validated Debian, RPM, and Flatpak artifacts before publishing them
- Remaining gap: the issue's requested before/after direct and transitive dependency counts plus clean-build timings are not yet recorded
- Next step: capture and publish the dependency/timing evidence, then reassess ISSUE:14 for closure without changing the proven package policy
- Public action: PR:8 merged and `v0.1.0` published; no issue comment, label, or closure
