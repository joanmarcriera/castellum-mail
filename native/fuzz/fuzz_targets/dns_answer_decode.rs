// Castellum - fuzz target for the account auto-detect DNS answer decoder.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Feeds arbitrary bytes to the hand-written SRV/MX response parser used by
// `castellum_autoconfig_lookup` (MM-14). The property under test is absolute:
// the parser must NEVER panic, loop or over-read, whatever a resolver (or a
// hostile network) returns. A decoded record list or an empty result are both
// fine outcomes.
//
// Run manually (not part of CI -- unbounded duration):
//   cd native/fuzz && cargo +nightly fuzz run dns_answer_decode
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = castellum_core::autoconfig::fuzz_dns_answer(data);
});
