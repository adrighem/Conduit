# ISSUE:14 — Dependencies and native-media packaging

- Status: open; implementation, dependency counts, and real release-package validation are complete, while clean-build timing evidence remains outstanding
- Confidence: high
- Impact: removes unused supply-chain/build surface and keeps unsupported native huddle capabilities out of general packages
- Intent: remove unused direct dependencies, measure the graph/build impact, and align Cargo, Meson, CI, and all package formats on one documented feature policy
- Relationship: PR:8 is merged and `v0.1.0` is public; ISSUE:14 is no longer a first-release blocker
- Resolution: removed three unused direct dependencies; regenerated the lockfile and Flatpak sources; disabled native media and screen sharing in Debian, RPM, and release Flatpak definitions; removed package-only media dependencies and capture permission; retained CI's opt-in feature matrix and the external Slack fallback
- Validation: final CI passes default and opt-in native-media configurations; Release run `29835203473` built, installed, and validated Debian, RPM, and Flatpak artifacts before publishing them
- Security follow-up: commit `280bec6`, pushed in main head `f0fe9ec`, removed inactive optional `quinn-proto` 0.11.14 metadata; Dependabot alert 1 changed to `fixed` automatically at 2026-07-29T11:32:04Z without dismissal
- Latest dependency evidence: direct dependencies change from 29 to 30 because Rustls provider ownership becomes explicit; total locked packages fall from 392 to 388, and lockfile-level transitive entries fall from 362 to 357, after removing six unused HTTP/3-related packages and adding the two actively used HTTP/2 packages
- Remaining gap: the requested clean-build timings are not yet recorded
- Next step: record clean-build timing evidence, remove PR:18's premature closing linkage, then reassess ISSUE:14 for closure without changing the proven native-media package policy
- Public action: PR:8 merged and `v0.1.0` published earlier; dependency cleanup commits pushed now; no issue comment, label, or closure
