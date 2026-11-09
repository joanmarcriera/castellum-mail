# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer, session 2 (second file): one more kept scenario + a clearer capture for the mark-all defect. Usage: python3 s3.py"""
from s2 import *

# --- 16. writes switched off again while a delete is still queued: nothing reaches the server ------
write("xf-readonly-again-holds-queued-delete", ["evaluation-mode", "delete", "offline", "replay", "write-safety"], [
    add("s"), writes("s"), mbox("s", "inbox"), IDLE,
    msg("XF s msg 1"), R("deleteSelected"), settle(600), drained(), IDLE, settle(1500),
    ep("s", "127.0.0.1:3999"), settle(1500),
    msg("XF s msg 4"), R("deleteSelected"), settle(1000), EL("deleted while the server is down: queued", ("reading:messageCount", "3"), ("reading:errorText", "none")),
    R("setWrite", accountRef="s", value=False), settle(1000),
    ep("s", "127.0.0.1:3143"), settle(1000), R("reSyncAll"), settle(7000), IDLE,
    mbox("s", "trash"), IDLE, settle(1500), cap("trash-while-read-only-again"),
    EL("server is back but the account is read-only again: the queued delete was NOT sent (Trash still holds only the first one)", ("reading:messageCount", "1"), ("reading:hasSubject:XF s msg 4", "false")),
    mbox("s", "inbox"), IDLE, settle(800), EL("Inbox unchanged, no error", ("reading:messageCount", "3"), ("reading:errorText", "none")),
    R("setWrite", accountRef="s", value=True), settle(1500), drained(60000), IDLE, settle(2500),
    mbox("s", "trash"), IDLE, waitR("hasSubject:XF s msg 4", "true"), EL("writes enabled again: the held delete goes through", ("reading:messageCount", "2")),
    mbox("s", "inbox"), IDLE, settle(800), EL("Inbox still 3", ("reading:messageCount", "3"), ("reading:errorText", "none")),
], timeout=300000)

write("xf-defect-mark-all-read-ignores-search", ["mark-all-read", "search", "read"], [
    add("q"), writes("q"), mbox("q", "inbox"), IDLE, q("XF q msg 2"), IDLE, settle(700),
    EL("the search lists one message", ("reading:messageCount", "1"), ("reading:hasSubject:XF q msg 2", "true")), cap("search-shows-one"),
    cmd("markAllRead"), settle(1000), drained(), q(""), IDLE, settle(700), cap("inbox-after-mark-all"),
    q("is:unread"), IDLE, settle(700),
    EL("Mark All as Read with a search showing 1 message must leave the 4 messages the search hid unread", ("reading:messageCount", "4")),
], timeout=300000)
