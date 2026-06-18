# Maintenance Patterns

## 2026-06-17

- No public backlog exists yet, so maintenance attention should focus on release readiness, onboarding clarity, issue templates, and CI coverage.
- The repository currently relies on README guidance for Slack app setup; future user reports may cluster around OAuth redirect setup, Socket Mode app-level tokens, and GNOME dependency installation.

## 2026-06-18

- Local repo health and GitHub Actions are green, so the highest-leverage maintenance work is planned UX architecture and documentation rather than reactive triage.
- Sidebar improvements should keep pure grouping/sorting behavior testable outside GTK widget construction to avoid growing `src/window.rs` into a maintenance hotspot.
