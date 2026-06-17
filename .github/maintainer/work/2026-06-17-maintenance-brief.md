# Maintenance Brief: 2026-06-17

## Current State

- GitHub backlog is empty: 0 issues and 0 pull requests returned by repository queries.
- Local CI-equivalent checks pass on the current working tree.
- The repository has no issue templates, PR template, contributing guide, or code of conduct.
- Local Clippy validation is not available in this environment.

## Top Recommendations

1. Add lightweight GitHub issue and pull request templates before the project attracts support traffic.
2. Add Clippy to CI after verifying the project is warning-clean under the GitHub toolchain.
3. Expand release-readiness docs around Slack app setup, Socket Mode tokens, Flatpak packaging, and screenshots.

## Why This Matters

- Empty backlog means the highest-leverage maintenance work is preventive: better incoming reports, stronger CI, and clearer onboarding.
- Slack setup is likely to be the first support bottleneck because OAuth redirect URLs, user-token PKCE scopes, and app-level Socket Mode tokens are easy to misconfigure.
- CI already validates core packaging files, so adding a lint gate would increase confidence without changing runtime behavior.

## Risks And Unknowns

- The working tree contains a large local simplification of Slack/auth/realtime code; recommendations are based on the current local tree, not a clean remote checkout.
- Clippy may expose warnings that require code changes before it can be added as a required CI gate.
- The maintainer skill package is missing its referenced scripts and docs, so this brief was produced manually.

