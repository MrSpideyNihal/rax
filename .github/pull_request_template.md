## What and why

## Evidence

<!-- Tests run. For performance changes: before and after data under benchmarks/results, plus the exact command. -->

## Checklist

- [ ] `cargo fmt`, `clippy -D warnings` and `cargo test` pass
- [ ] Real-guest tests pass, if WHP code changed
- [ ] Every new `unsafe` block has a `SAFETY` comment
- [ ] Docs and ADRs updated where behaviour or decisions changed
- [ ] Nothing here bypasses licensing, DRM or Play Integrity (ADR-0005)
