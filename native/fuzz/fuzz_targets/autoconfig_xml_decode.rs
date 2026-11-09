// Castellum - fuzz target for the account auto-detect autoconfig XML reader.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Feeds arbitrary bytes to the config-v1.1.xml reader used by
// `castellum_autoconfig_lookup` (MM-14). The documents come from a domain's own
// web host or Mozilla's ISPDB, i.e. untrusted input. Properties: never panics,
// and any `Found` result carries a syntactically valid host and a non-zero port
// (the assert lives in `fuzz_autoconfig_xml`'s return value).
//
// Run manually (not part of CI -- unbounded duration):
//   cd native/fuzz && cargo +nightly fuzz run autoconfig_xml_decode
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = castellum_core::autoconfig::fuzz_autoconfig_xml(data);
});
