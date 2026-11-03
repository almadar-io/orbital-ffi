<div align="center">

# orbital-ffi

**The one native library both mobile shells link: the renderer C ABI, the kernel C ABI and the JNI forwarders.**

[![CI](https://github.com/almadar-io/orbital-ffi/actions/workflows/ci.yml/badge.svg)](https://github.com/almadar-io/orbital-ffi/actions/workflows/ci.yml)
[![License: BUSL-1.1](https://img.shields.io/badge/license-BUSL--1.1-blue)](LICENSE)

</div>

`orbital-ffi` packages the embedded kernel and the canvas renderer behind a C ABI so iOS (SwiftUI) and Android (Jetpack Compose) hosts run the same kernel as every other host. It builds as `staticlib` (iOS), `cdylib` (Android) and `rlib`. It marshals; every decision is the kernel's.

## Where this fits

```mermaid
flowchart TB
  subgraph kernel["Kernel and substrate"]
    core["orbital-core<br/>IR · evaluator · kernel"]
    canvas["orbital-canvas<br/>2D/3D render primitives"]
  end
  subgraph lang["Language"]
    lolo["orbital-lolo<br/>.lolo → .orb"]
    compiler["orbital-compiler<br/>validate · codegen"]
  end
  subgraph hosts["Shells and hosts"]
    ts["orbital-shell-typescript<br/>web · node · @almadar/core"]
    rs["orbital-shell-rust<br/>desktop · tui"]
    py["orbital-shell-python"]
    ios["orbital-shell-ios"]
    android["orbital-shell-android"]
    os["orbital-shell-os"]
    wasm["orbital-wasm<br/>browser kernel"]
    ffi["orbital-ffi<br/>C ABI for mobile"]
    router["orbital-router"]
  end
  cli["orbital-cli<br/>the orb binary"]
  ws["orbital-rust<br/>workspace · cross-crate tests"]
  dist["orb<br/>npm · releases"]
  subgraph pkgs["Language packages"]
    std["almadar-std<br/>@almadar/std substrate"]
    beh["behaviors<br/>themed organisms"]
    langs["lolo-ar · lolo-sl<br/>language packs"]
  end
  subgraph apps["Apps and sites"]
    builder["builder (studio)"]
    kflow["kflow-academy"]
    nata["nata"]
    rawi["rawi"]
    site["almadar (websites)"]
  end
  core --> lolo & compiler & ts & rs & py & ios & android & os & wasm & ffi & router
  canvas --> ts & rs & ffi
  lolo --> compiler
  compiler --> cli
  ts & rs & py & ios & android & os --> cli
  cli --> dist
  ws -.pins.-> core & lolo & compiler & cli
  dist --> std
  std --> beh
  std --> langs
  beh --> builder & kflow & nata & rawi & site
  click core "https://github.com/almadar-io/orbital-core"
  click canvas "https://github.com/almadar-io/orbital-canvas"
  click lolo "https://github.com/almadar-io/orbital-lolo"
  click compiler "https://github.com/almadar-io/orbital-compiler"
  click ts "https://github.com/almadar-io/orbital-shell-typescript"
  click rs "https://github.com/almadar-io/orbital-shell-rust"
  click py "https://github.com/almadar-io/orbital-shell-python"
  click ios "https://github.com/almadar-io/orbital-shell-ios"
  click android "https://github.com/almadar-io/orbital-shell-android"
  click os "https://github.com/almadar-io/orbital-shell-os"
  click wasm "https://github.com/almadar-io/orbital-wasm"
  click ffi "https://github.com/almadar-io/orbital-ffi"
  click router "https://github.com/almadar-io/orbital-router"
  click cli "https://github.com/almadar-io/orbital-cli"
  click ws "https://github.com/almadar-io/orbital-rust"
  click dist "https://github.com/almadar-io/orb"
  click std "https://github.com/almadar-io/almadar-std"
  click beh "https://github.com/almadar-io/behaviors"
  click langs "https://github.com/almadar-io/lolo-ar"
  click builder "https://github.com/itechdom/almadar-studio"
  click kflow "https://github.com/almadar-io/almadar-kflow-academy"
  click nata "https://github.com/almadar-io/almadar-nata"
  click rawi "https://github.com/almadar-io/almadar-rawi"
  click site "https://github.com/almadar-io/almadar"
  classDef here fill:#2563eb,color:#fff,stroke:#1e40af,stroke-width:3px
  class ffi here
```

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

- [Orbital Core](https://github.com/itechdom/kflow.ai/blob/main/docs/Almadar_Orbital_Core.md)
- [Orbital Shell iOS](https://github.com/itechdom/kflow.ai/blob/main/docs/Almadar_Orbital_Shell_iOS.md) and [Orbital Shell Android](https://github.com/itechdom/kflow.ai/blob/main/docs/Almadar_Orbital_Shell_Android.md)
- [Execution Semantics](https://github.com/itechdom/kflow.ai/blob/main/docs/Almadar_Execution_Semantics.md)

## License

[Business Source License 1.1](LICENSE): free for non-commercial use and internal evaluation; commercial production use needs a licence from Almadar FZE. Converts to Apache-2.0 on 2030-02-01.
