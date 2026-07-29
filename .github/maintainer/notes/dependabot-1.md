# Dependabot alert 1 - GHSA-4w2j-m93h-cj5j

- Status: fixed automatically on remote `main` at 2026-07-29T11:32:04Z
- Package: `quinn-proto` 0.11.14 in `Cargo.lock` and the Flatpak Cargo source list
- Advisory: remote memory exhaustion through unbounded out-of-order stream reassembly
- Patched version: 0.11.15
- Exposure: low based on current evidence; `quinn` was an optional Reqwest HTTP/3 dependency and neither the default nor all-feature Conduit tree compiled it
- Cause: Reqwest's `rustls` convenience feature records a weak optional Quinn provider edge in the lockfile, and the Flatpak generator vendors every locked registry source
- Resolution: commit `280bec6` uses `rustls-no-provider`, selects AWS-LC directly, and enables stable HTTP/2; Quinn and its related unused packages are absent from both generated files
- Validation: Slack accepted direct HTTP/3 probes, but Reqwest 0.13 HTTP/3 is experimental and forced-only; Slack negotiated HTTP/2 through the resulting Conduit dependency configuration
- Remote validation: Dependabot fixed the alert after its dependency graph refresh; `dismissed_at` and `auto_dismissed_at` remain null
- Next step: keep HTTP/3 disabled until Reqwest provides stable negotiation and safe fallback
- Public action: commits `280bec6` and `f0fe9ec` pushed with explicit approval; no alert dismissal
