<div align="center">

# orbital-ffi

**The one native library both mobile shells link: the renderer C ABI, the kernel C ABI and the JNI forwarders.**

[![CI](https://github.com/almadar-io/orbital-ffi/actions/workflows/ci.yml/badge.svg)](https://github.com/almadar-io/orbital-ffi/actions/workflows/ci.yml)
[![License: BUSL-1.1](https://img.shields.io/badge/license-BUSL--1.1-blue)](LICENSE)

</div>

<p align="center"><sub>Part of <a href="https://github.com/almadar-io/orb"><b>Orb</b></a>: the language where the rule is the program. <a href="https://orb.almadar.io">orb.almadar.io</a></sub></p>

`orbital-ffi` packages the embedded kernel and the canvas renderer behind a C ABI so iOS (SwiftUI) and Android (Jetpack Compose) hosts run the same kernel as every other host. It builds as `staticlib` (iOS), `cdylib` (Android) and `rlib`. It marshals; every decision is the kernel's.

## Where this fits

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/almadar-io/orb/main/static/img/readme/architecture/orbital-ffi-dark.svg">
  <img src="https://raw.githubusercontent.com/almadar-io/orb/main/static/img/readme/architecture/orbital-ffi-light.svg" width="100%" alt="Where orbital-ffi sits in the Almadar architecture">
</picture>

| Upstream (this repo depends on) | Downstream (depends on this repo) |
|---|---|
| [orbital-core](https://github.com/almadar-io/orbital-core): the embedded kernel | [orbital-shell-ios](https://github.com/almadar-io/orbital-shell-ios): links the static library |
| [orbital-canvas](https://github.com/almadar-io/orbital-canvas): the renderer, built with its `http` feature | [orbital-shell-android](https://github.com/almadar-io/orbital-shell-android): links the shared library through JNI |

## Quick start

The crate reads the Almadar language data from installed npm packages, so install those first. The toolchain is pinned to Rust 1.93.0 in `rust-toolchain.toml`.

```bash
npm install --no-save --no-package-lock @almadar/core@^10 @almadar/std@^16 @almadar/lolo-ar@^1 @almadar/lolo-sl@^1
cargo test
```

## What's inside

| Path | Purpose |
|---|---|
| `src/kernel_c_api.rs` | The kernel C ABI (over `orbital_core::embed`) |
| `src/lib.rs` | Re-exports `orbital_canvas::c_api`, the renderer C ABI |
| `src/jni_bridge.rs`, `src/jni_kernel_bridge.rs`, `src/jni_util.rs` | JNI forwarders for Android (string and array marshalling) |
| `tests/` | Fixtures, including resolved schemas the kernel tests boot from |
| `benches/c_abi_fixtures.rs` | C ABI benchmark |

The device libraries are built from the orbital-rust workspace with `make ios-lib` and `make android-lib`; see the workspace `Makefile`.

## Development

- Test with `cargo test`; CI (`.github/workflows/ci.yml`) runs it on push and pull request to `main`.
- Open gaps are tracked in [`Almadar_Gaps.md`](Almadar_Gaps.md). Work on `main` only; there are no feature branches.
- Inside the monorepo this repo is the submodule `orbital-rust/crates/orbital-ffi`.

## Docs

- [Orb documentation](https://orb.almadar.io/docs): the language, the `orb` CLI and how the pieces fit

## License

[Business Source License 1.1](LICENSE): free for non-commercial use and internal evaluation; commercial production use needs a licence from Almadar FZE. Converts to Apache-2.0 on 2030-02-01.
