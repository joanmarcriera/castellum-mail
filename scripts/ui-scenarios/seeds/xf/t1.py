# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
K = ["reading:selectedSubject", "reading:messageCount", "ext:pendingCount", "reading:subjects"]
write("xf-tmp-probe", ["probe"], [
    add("c"), writes("c"), mbox("c", "inbox"), IDLE, msg("XF c msg 3"), IDLE, dump(*K),
    cmd("archive"), settle(400), dump(*K), cap("after-archive"),
    drained(), IDLE, settle(1500), dump(*K),
    msg("XF c msg 1"), R("deleteSelected"), settle(400), dump(*K), drained(), IDLE, settle(1500),
    mbox("c", "trash"), IDLE, dump(*K, "reading:mailboxRole"),
    msg("XF c msg 1"), reg("sheet:pd"), reg("pd"), R("deleteSelected"), settle(800),
    dump("reading:pendingDelete"), {"op": "assert", "expect": [{"sheetAttached": True}]}, capall("perm-dialog"),
    R("cancelDelete"), settle(600), dump("reading:pendingDelete", *K),
    mbox("c", "inbox"), IDLE, R("searchScope", text="account"), {"op": "search", "text": "XF c"}, IDLE, settle(800),
    dump(*K, "reading:mailboxRole"), cap("search-account"),
    msg("XF c msg 1"), R("deleteSelected"), settle(1200), dump("reading:pendingDelete", "reading:errorText", *K), capall("delete-trashed-from-search"),
], timeout=300000)
