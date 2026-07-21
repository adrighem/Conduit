# PR:8 — Release 0.1.0

- Status: hold; clean and fully green, pending manual first-release checks and explicit merge approval
- Confidence: high
- Provenance: same-repository Release Please branch with one GitHub-verified bot commit plus maintainer commit `517a45e` correcting generated release-note attribution
- Diff: release manifest bootstrap, `CHANGELOG.md`, and AppStream release date only; no workflows, dependencies, binaries, install scripts, or production code changed
- Blockers:
  - the manual first-release checklist remains incomplete
  - real Debian, RPM, and Flatpak build/install validation runs only after merge and remains unproven
- Resolved gates: valid bootstrap manifest handling is on `main`; ISSUE:14 is implemented; Flatpak install validation gates publication; the false ISSUE:7 attribution is removed from both changelog and PR body
- Validation: corrected head `517a45e` is clean and mergeable; CI runs `29825962522` and `29825965362` plus CodeQL `29825963617` all pass, including strict Clippy, 546 default tests, optional native-media checks, and both Meson configurations
- Merge impact: merging creates the first draft release and starts Debian, RPM, and Flatpak publication jobs; those package jobs have never completed for a real release
- Public action: CI approved, changelog branch and PR body corrected; no merge or release
