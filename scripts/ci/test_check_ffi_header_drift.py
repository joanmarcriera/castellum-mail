#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Self-test for check-ffi-header-drift.py. stdlib unittest only — no pytest
dependency added for one script.

Run: python3 scripts/ci/test_check_ffi_header_drift.py
"""
import importlib.util
import pathlib
import unittest

SCRIPT_PATH = pathlib.Path(__file__).parent / "check-ffi-header-drift.py"
_spec = importlib.util.spec_from_file_location("check_ffi_header_drift", SCRIPT_PATH)
module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(module)


class RustFnNamesTest(unittest.TestCase):
    def test_extracts_a_simple_no_mangle_function(self):
        text = '#[no_mangle]\npub extern "C" fn castellum_version() -> *mut c_char {\n'
        self.assertEqual(module.rust_fn_names(text), {"castellum_version"})

    def test_ignores_a_pub_extern_fn_without_no_mangle(self):
        text = 'pub extern "C" fn not_exported() {}\n'
        self.assertEqual(module.rust_fn_names(text), set())

    def test_extracts_multiple_functions(self):
        text = (
            '#[no_mangle]\npub extern "C" fn castellum_a() {}\n\n'
            '#[no_mangle]\npub extern "C" fn castellum_b(x: i32) -> i32 { x }\n'
        )
        self.assertEqual(module.rust_fn_names(text), {"castellum_a", "castellum_b"})

    def test_extracts_a_function_with_an_intervening_allow_attribute(self):
        # The real repo shape for 62 of 71 functions today: a second
        # attribute line sits between #[no_mangle] and the fn itself. This is
        # the regression test for the bug where the checker only found the 9
        # functions with no second attribute and reported the other 62 as
        # false-positive drift.
        text = (
            '#[no_mangle]\n#[allow(clippy::not_unsafe_ptr_arg_deref)]\n'
            'pub extern "C" fn castellum_open_profile(path_utf8: *const c_char) -> u64 {}\n'
        )
        self.assertEqual(module.rust_fn_names(text), {"castellum_open_profile"})


class HeaderFnNamesTest(unittest.TestCase):
    def test_extracts_a_single_line_declaration(self):
        text = "void castellum_string_free(char *s);\n"
        self.assertEqual(module.header_fn_names(text), {"castellum_string_free"})

    def test_extracts_a_declaration_wrapped_across_lines(self):
        text = (
            "int32_t castellum_list_threads(uint64_t handle, const char *account_id,\n"
            "                               int64_t limit, int64_t offset, char **out_json);\n"
        )
        self.assertEqual(module.header_fn_names(text), {"castellum_list_threads"})

    def test_ignores_a_function_name_mentioned_only_in_a_comment(self):
        text = (
            "/* Frees a string previously returned by castellum_version(). */\n"
            "void castellum_string_free(char *s);\n"
        )
        self.assertEqual(module.header_fn_names(text), {"castellum_string_free"})


class MainIntegrationTest(unittest.TestCase):
    def test_a_rust_only_function_is_detected_as_drift(self):
        rust = module.rust_fn_names(
            '#[no_mangle]\npub extern "C" fn castellum_a() {}\n'
            '#[no_mangle]\npub extern "C" fn castellum_new() {}\n'
        )
        header = module.header_fn_names("void castellum_a(void);\n")
        self.assertEqual(rust - header, {"castellum_new"})

    def test_a_header_only_function_is_detected_as_drift(self):
        rust = module.rust_fn_names('#[no_mangle]\npub extern "C" fn castellum_a() {}\n')
        header = module.header_fn_names("void castellum_a(void);\nvoid castellum_stale(void);\n")
        self.assertEqual(header - rust, {"castellum_stale"})

    def test_matching_sets_are_equal(self):
        rust = module.rust_fn_names('#[no_mangle]\npub extern "C" fn castellum_a() {}\n')
        header = module.header_fn_names("void castellum_a(void);\n")
        self.assertEqual(rust, header)


if __name__ == "__main__":
    unittest.main()
