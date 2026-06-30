# Rust Code Style

- Prefer small pure helper functions when behavior needs unit tests.
- Keep async network code isolated from UI code.
- Use `anyhow::Context` at IO and network boundaries.
- Avoid logging sensitive values.
- Run `cargo test` and `cargo check` before committing.
