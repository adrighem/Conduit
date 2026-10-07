# Conduit Project Guidance

- Conduit intentionally supports one connected Slack workspace. Treat single-workspace operation as a product boundary, not as a temporary limitation or a future multi-workspace roadmap gap.
- Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` before every push. Enforce via `.git/hooks/pre-push`.
