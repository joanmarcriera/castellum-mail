#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Build the Castellum Rust static library that the Xcode app links against.
#
#   bash apple/scripts/build-rust.sh              # host arch (fast; for dev + CI tests)
#   bash apple/scripts/build-rust.sh --universal  # arm64 + x86_64 lipo (for distribution)
#
# Debug builds use ONLY_ACTIVE_ARCH=YES, so a host-arch library is all a local
# build or the unsigned CI job needs. A DISTRIBUTION build must be universal —
# and must FAIL rather than quietly fall back, because an arm64-only staticlib
# links fine on Apple Silicon and produces a DMG that simply does not run on
# Intel. That failure is invisible until a user reports it, so --universal
# installs the missing Rust targets and errors if it still cannot produce both.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT/native"

UNIVERSAL=0
[[ "${1:-}" == "--universal" ]] && UNIVERSAL=1

build_universal() {
  echo "[build-rust] distribution build: arm64 + x86_64…"
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    if ! rustup target list --installed | grep -qx "$target"; then
      # Braces required: the ellipsis that follows is multi-byte, and bash
      # swallows it into the variable name without them (set -u then aborts).
      echo "[build-rust] installing missing Rust target ${target}…"
      rustup target add "$target"
    fi
    cargo build --release -p castellum-ffi --target "$target"
  done

  mkdir -p target/universal/release
  lipo -create \
    target/x86_64-apple-darwin/release/libcastellum_ffi.a \
    target/aarch64-apple-darwin/release/libcastellum_ffi.a \
    -output target/universal/release/libcastellum_ffi.a

  # Prove it, rather than trusting that lipo did what was asked: this is the
  # single check standing between us and an Intel-broken release.
  local archs
  archs="$(lipo -archs target/universal/release/libcastellum_ffi.a)"
  for want in arm64 x86_64; do
    grep -qw "$want" <<<"$archs" || {
      echo "[build-rust] FATAL: universal library is missing $want (got: $archs)" >&2
      exit 1
    }
  done
  echo "[build-rust] universal library ($archs) -> native/target/universal/release/libcastellum_ffi.a"
}

if [[ "$UNIVERSAL" == "1" ]]; then
  build_universal
  exit 0
fi

echo "[build-rust] building castellum-ffi (release, host arch)…"
cargo build --release -p castellum-ffi

# Convenience only: if both targets happen to be installed, refresh the
# universal artefact too, since LIBRARY_SEARCH_PATHS prefers it and a stale one
# would silently be linked instead of the build we just made.
installed="$(rustup target list --installed 2>/dev/null || true)"
if grep -q x86_64-apple-darwin <<<"$installed" && grep -q aarch64-apple-darwin <<<"$installed"; then
  build_universal
else
  echo "[build-rust] host-arch only — use --universal for a distribution build."
  echo "[build-rust] library -> native/target/release/libcastellum_ffi.a"
  # A stale universal library from an earlier run outranks this one in
  # LIBRARY_SEARCH_PATHS, so it must not be left lying around.
  if [[ -f target/universal/release/libcastellum_ffi.a ]]; then
    echo "[build-rust] removing stale universal library so this build is the one linked."
    rm -f target/universal/release/libcastellum_ffi.a
  fi
fi
