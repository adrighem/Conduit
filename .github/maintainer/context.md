# Maintainer Context

## Project

Conduit is a lightweight GNOME desktop client for Slack written in Rust with GTK4 and libadwaita.

## Product Boundaries

- Conduit is intentionally a single-Slack-workspace application. Treat one connected workspace as product scope, not as a temporary limitation or an implied future multi-workspace roadmap item.

## Current Priorities

- Keep CI green across Rust tests, formatting, Meson build, desktop file validation, schema validation, and AppStream validation.
- Prefer stable GNOME desktop behavior and clear Slack setup documentation over broad feature expansion.
- Reduce future support burden around Slack OAuth, Socket Mode setup, packaging, and release readiness.

## Public Tone

- Be concise, factual, and friendly.
- Ask for reproduction details only when they materially change the next step.
- Avoid public actions without explicit maintainer approval.
