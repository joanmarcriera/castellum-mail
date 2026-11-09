# Castellum native core (Rust) + macOS app (Swift)

This is the Castellum stack — a Rust core library exposed to a Swift/AppKit
macOS UI over a narrow C ABI. It was built in independently-verifiable slices
alongside the original Electron/TypeScript app (under `../src`); that app was
removed at tag `electron-final` once the native app reached parity. 
> **Greenfield:** the app is pre-release with no users and disposable local data, so
> there is **no data migration** — the native app re-syncs from IMAP into a fresh
> schema. Nothing here reads or mutates the Electron app's data.

## Layout

```
native/                     Rust Cargo workspace
  crates/castellum-models/  dependency-free DTOs + CastellumStatus (ABI codes)
  crates/castellum-core/    domain logic: version / open_profile / close_profile
  crates/castellum-ffi/     C ABI (staticlib+cdylib); include/castellum.h
../apple/                   XcodeGen macOS app that links the Rust staticlib
```

Target crate roadmap (added slice by slice, not all present yet):
`castellum-db`, `castellum-mail`, `castellum-providers`, `castellum-provider-imap`,
`castellum-provider-smtp`, `castellum-search`, `castellum-ai`, `castellum-ai-ollama`,
`castellum-security`.

## Prerequisites

- Rust stable via `rustup` (developed with cargo 1.96).
- Xcode + Command Line Tools (developed with Xcode 26.6 / Swift 6.3) — for `swift`,
  `clang`, and the CoreFoundation/Security frameworks the Rust staticlib links.
- `xcodegen` (`brew install xcodegen`) to generate the Xcode project.

## Build, test, run

```bash
# Rust core + FFI (from repo root)
cargo fmt   --all --check          --manifest-path native/Cargo.toml
cargo clippy --all-targets -- -D warnings  # run inside native/, or -p per crate
cargo test                          --manifest-path native/Cargo.toml   # unit + tests/ffi.rs
cargo build --release -p castellum-ffi --manifest-path native/Cargo.toml  # -> libcastellum_ffi.a

# macOS app (links the Rust staticlib)
bash apple/scripts/build-rust.sh                     # build staticlib (+universal if targets present)
xcodegen generate --spec apple/project.yml
xcodebuild -project apple/Castellum.xcodeproj -scheme Castellum \
  -configuration Debug -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build

# The app prints an FFI smoke line to stderr on launch, e.g.:
#   [castellum-smoke] core=0.1.0 open_rc=0 handle=1 close_rc=0 dir=/var/folders/.../castellum-smoke-...
```


## The C ABI (slice 1)

`native/crates/castellum-ffi/include/castellum.h` — four functions:
`castellum_version`, `castellum_string_free`, `castellum_open_profile`,
`castellum_close_profile`. Handles are opaque `u64` keys (0 = invalid). Rust owns
strings it returns — free them with `castellum_string_free`.

> **Note:** the header is **hand-written** for this small surface (offline-safe, no
> extra crates). A later slice may adopt `cbindgen` (via `build.rs`) once the exported
> surface grows richer types; if so, add a CI check that the committed header is fresh.

## Boundaries (do not cross)

- The FFI exposes small typed commands only — never provider objects, raw SQLite
  handles, IMAP sessions, AI clients, or Electron preload channel names.
- Rust owns durable state; Swift owns window/selection/presentation state.
- No Tantivy / llama.cpp / MLX / CoreML / Gmail API / Graph / JMAP until the core and
  macOS app are proven (see the decision log).
