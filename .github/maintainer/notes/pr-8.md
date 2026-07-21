# PR:8 — Release 0.1.0

- Status: merged on 2026-07-21 as squash commit `49e9203`; `v0.1.0` is published
- Confidence: high
- Provenance: same-repository Release Please branch with one GitHub-verified bot commit plus maintainer commit `6a6d220` correcting release-note attribution and stabilizing the headless shortcut test
- Diff at merge: release manifest bootstrap, `CHANGELOG.md`, AppStream release date, and the focused headless test fix; no dependency or install-script changes
- Resolved gates: exact PR-head CI and CodeQL passed, generated issue references did not close ISSUE:14, and credential rotation was accepted as complete by the operator
- Merge result: Release run `29830697395` created the draft and validated Debian and Flatpak, then exposed the RPM runner defects addressed by the guarded recovery fixes
- Final validation: CI `29835081152`, CodeQL `29835080623`, and guarded Release `29835203473` all pass on `8c16452`; the public Debian, RPM, and Flatpak hashes match `SHA256SUMS`
- Public action: PR:8 merged and `v0.1.0` published; ISSUE:14 remains open and received no comment, label, or closure
