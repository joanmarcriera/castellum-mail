// Castellum - fuzz target for the IMAP response decoder.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Feeds arbitrary bytes to imap-codec's ResponseCodec (and GreetingCodec, the
// other server-controlled decode entry point Session uses). The property under
// test is absolute: decoding must NEVER panic, for any input — that is the
// entire reason the imap/imap-proto stack was replaced (see
// project-docs/imap-crate-migration-2026-09-17.md and CLAUDE.md gotcha 1). A
// successful decode or a typed Err are both fine outcomes.
//
// Run manually (not part of CI — unbounded duration):
//   cd native/fuzz && cargo +nightly fuzz run response_decode
#![no_main]

use imap_codec::decode::Decoder;
use imap_codec::{GreetingCodec, ResponseCodec};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ResponseCodec::default().decode(data);
    let _ = GreetingCodec::default().decode(data);
});
