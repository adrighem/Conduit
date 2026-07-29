# ISSUE:14 - Dependencies and native-media packaging

- Status: closed as completed on 2026-07-29
- Confidence: high
- Impact: removes unused supply-chain/build surface and keeps unsupported native huddle capabilities out of general packages
- Intent: remove unused direct dependencies, measure the graph/build impact, and align Cargo, Meson, CI, and all package formats on one documented feature policy
- Relationship: PR:8 is merged and `v0.1.0` is public; ISSUE:14 is no longer a first-release blocker
- Resolution: removed three unused direct dependencies; regenerated the lockfile and Flatpak sources; disabled native media and screen sharing in Debian, RPM, and release Flatpak definitions; removed package-only media dependencies and capture permission; retained CI's opt-in feature matrix and the external Slack fallback
- Validation: final CI passes default and opt-in native-media configurations; Release run `29835203473` built, installed, and validated Debian, RPM, and Flatpak artifacts before publishing them
- Security follow-up: commit `280bec6`, now included in main, removed inactive optional `quinn-proto` 0.11.14 metadata; Dependabot alert 1 changed to `fixed` automatically at 2026-07-29T11:32:04Z without dismissal
- Dependency evidence: the controlled issue comparison falls from 33 direct, 400 transitive, and 434 total packages to 30 direct, 361 transitive, and 392 total; current main after the HTTP follow-up has 31 direct, 356 transitive, and 388 total
- Clean-build evidence: three offline clean builds per controlled ref show 9.53% lower median user CPU, 6.28% lower system CPU, and 29 fewer Cargo `Compiling` entries; elapsed ranges overlap, so no wall-clock improvement is claimed
- Remaining gap: none
- Remote validation: evidence commit `1b1b972` passed CI `30464468686`, CodeQL `30464466642`, and guarded release automation `30465162150`
- Resolution action: closed ISSUE:14 as completed with a short link to the recorded dependency audit
- Release follow-up: change PR:18's generated closing linkage to a non-closing reference because the issue is already resolved independently
