# Contributing

Thanks for helping. Read the [feasibility report](docs/FEASIBILITY.md) first, as it explains what the project can and cannot do.

## Before you submit

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

On a machine with WHP enabled, also run the real-guest tests, which CI cannot:

```powershell
cargo test -p rax-host --test whp_smoke -- --ignored
./scripts/fetch-guest.ps1
cargo test --release -p rax-vmm --test boot_linux -- --ignored
```

## Rules

- **Performance claims need evidence.** Include raw results under `benchmarks/results` and the exact command.
- **Compatibility claims need a test.** State the hardware and the versions.
- **`unsafe` stays in `rax-host`.** Every block needs a `// SAFETY:` comment explaining why it is sound.
- **New dependencies need a reason.** `cargo deny` must pass.
- **Significant decisions need an ADR** in `docs/ADR`.
- **Do not add empty modules** for planned work.

## Not accepted

Anything that bundles or installs Google apps, spoofs device identity or certification, hides root, or bypasses licensing, DRM, or Play Integrity. See [ADR-0005](docs/ADR/0005-google-play-policy.md). Also not accepted: extracted proprietary binaries, such as ARM translators or code from closed-source emulators.

## Licensing

Unless stated otherwise, contributions are licensed under Apache-2.0, as described in section 5 of the [licence](LICENSE).
