# Dependency Audit

This document records the dependency-count and clean-build evidence requested by
[issue #14](https://github.com/adrighem/Conduit/issues/14).

## Controlled comparison

The issue named `b93e743` as its baseline. Its `Cargo.toml` and `Cargo.lock` are
byte-identical to the controlled before ref `1e6828c`, which is the parent of the
dependency-removal commit. The controlled pair is:

- Before: `1e6828cb6104430673216d25ad2b928961840b31`
- After: `af7ed578302f37f013c441de5f49ef90adef69d2`

These refs have identical Rust source and default Meson build configuration.
Their relevant difference is the removal of `pulldown-cmark`, `slack-blocks`,
and `slack-morphism` plus the resulting lockfile update.

Counts use the root package dependency array in `Cargo.lock` for direct
dependencies and `[[package]]` blocks for the total. Transitive entries are
calculated as total packages minus the root package and direct dependencies.
This includes the renamed `adw` dependency.

| Graph | Direct | Transitive | Total |
| --- | ---: | ---: | ---: |
| Before | 33 | 400 | 434 |
| After | 30 | 361 | 392 |
| Change | -3 | -39 | -42 |

Current main at `30e4149` includes the later HTTP transport cleanup from
`280bec6` and has 31 direct dependencies, 356 transitive entries, and 388 total
packages. The direct count increases by one because Rustls provider ownership
is explicit, while the unused Quinn stack is removed.

## Clean-build method

Measurements were recorded on 2026-07-29 with:

- Debian 13 and Linux 7.1.4-native-xanmod1 on x86_64
- 13th Gen Intel Core i7-1355U, 12 logical CPUs
- 33,313,361,920 bytes of RAM
- Cargo 1.95.0 and rustc 1.95.0
- scratch targets on the `/home` Btrfs filesystem

The before dependency set was vendored outside the timed phase and used as the
shared offline source for both refs. Each run used a new target directory, 12
jobs, disabled incremental compilation, no default features, and:

```sh
cargo build --locked --offline --no-default-features
```

Runs were interleaved as before, after, after, before, before, after. No network
update or download occurred during a timed command.

| Sample | Elapsed s | User CPU s | System CPU s | Max RSS KiB |
| --- | ---: | ---: | ---: | ---: |
| Before 1 | 250.48 | 1015.81 | 88.33 | 1876412 |
| After 1 | 226.98 | 866.78 | 76.31 | 1866736 |
| After 2 | 255.03 | 876.56 | 81.55 | 1824776 |
| Before 2 | 216.79 | 958.04 | 81.42 | 1864312 |
| Before 3 | 178.55 | 896.29 | 76.34 | 1929964 |
| After 3 | 175.43 | 768.29 | 67.57 | 1893164 |

| Metric | Before median and range | After median and range | Median change |
| --- | ---: | ---: | ---: |
| Elapsed | 216.79 s (178.55-250.48) | 226.98 s (175.43-255.03) | +4.70% |
| User CPU | 958.04 s (896.29-1015.81) | 866.78 s (768.29-876.56) | -9.53% |
| System CPU | 81.42 s (76.34-88.33) | 76.31 s (67.57-81.55) | -6.28% |
| Max RSS | 1876412 KiB (1864312-1929964) | 1866736 KiB (1824776-1893164) | -0.52% |

Cargo emitted 263 `Compiling` entries before and 234 after, a reduction of 29
or 11.0%. Every paired after run used less user CPU. Elapsed ranges overlap
heavily, so this host does not provide evidence for a reliable wall-clock
speedup. The supported conclusion is a smaller dependency graph and less
compiler CPU work.
