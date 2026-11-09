#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Castellum - fail CI if the hand-written FFI header drifts from the Rust
extern "C" surface (S38).

Compares the set of `#[no_mangle] pub extern "C" fn` names in
native/crates/castellum-ffi/src/lib.rs against the set of `castellum_*`
function names DECLARED (not just mentioned in a comment) in
native/crates/castellum-ffi/include/castellum.h. Prints the exact mismatch
and exits 1 on any difference; exits 0 silently otherwise.

Usage: scripts/ci/check-ffi-header-drift.py [--rust PATH] [--header PATH]
"""
import argparse
import re
import sys

RUST_FN_RE = re.compile(
    r'#\[no_mangle\]\s*\n(?:#\[[^\n]*\]\s*\n)*pub extern "C" fn ([A-Za-z_][A-Za-z0-9_]*)'
)
HEADER_FN_RE = re.compile(r'\bcastellum_[A-Za-z0-9_]+(?=\s*\()')
BLOCK_COMMENT_RE = re.compile(r"/\*.*?\*/", re.DOTALL)


def rust_fn_names(text: str) -> set[str]:
    """Every #[no_mangle] pub extern "C" fn name in `text`.

    62 of the 71 real functions in castellum-ffi/src/lib.rs carry a second
    attribute (`#[allow(clippy::not_unsafe_ptr_arg_deref)]`) on the line
    between `#[no_mangle]` and `pub extern "C" fn` — the regex must skip zero
    or more such attribute lines, not require `pub extern` on the very next
    line, or it only finds the 9 functions with no second attribute and
    reports the other 62 as false-positive drift. Confirmed against the real
    file: a naive no_mangle-then-pub-extern-on-the-next-line pattern finds 9/71."""
    return set(RUST_FN_RE.findall(text))


def header_fn_names(text: str) -> set[str]:
    """Every castellum_* name immediately followed by '(' in `text`, after
    stripping /* ... */ comments — the header's own doc comments mention
    function names in prose (e.g. "Frees a string previously returned by
    castellum_version()."), which must not count as a declaration."""
    stripped = BLOCK_COMMENT_RE.sub("", text)
    return set(HEADER_FN_RE.findall(stripped))


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rust", default="native/crates/castellum-ffi/src/lib.rs")
    parser.add_argument("--header", default="native/crates/castellum-ffi/include/castellum.h")
    args = parser.parse_args(argv)

    with open(args.rust, encoding="utf-8") as f:
        rust_names = rust_fn_names(f.read())
    with open(args.header, encoding="utf-8") as f:
        header_names = header_fn_names(f.read())

    missing_from_header = sorted(rust_names - header_names)
    missing_from_rust = sorted(header_names - rust_names)

    if not missing_from_header and not missing_from_rust:
        print(f'OK: {len(rust_names)} extern "C" functions, header in sync.')
        return 0

    print(f"FFI header drift detected between {args.rust} and {args.header}", file=sys.stderr)
    if missing_from_header:
        print(f"  declared in Rust but missing from the header ({len(missing_from_header)}):", file=sys.stderr)
        for name in missing_from_header:
            print(f"    - {name}", file=sys.stderr)
    if missing_from_rust:
        print(f"  declared in the header but missing from Rust ({len(missing_from_rust)}):", file=sys.stderr)
        for name in missing_from_rust:
            print(f"    - {name}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
