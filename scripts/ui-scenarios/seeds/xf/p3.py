# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer, session 2 probe 3. Usage: python3 p3.py"""
from p2 import *
wprobe("xf-tmp-p3", [
    add("m"), add("n"), writes("m"), mbox("m", "inbox"), IDLE,
    msg("XF m msg 5"), IDLE, settle(3000), drained(), D("reading:selectedUnread"),
    cmd("toggleRead"), settle(3500), D("reading:selectedUnread"), drained(),
    msg("XF m msg 4"), IDLE, settle(3000), msg("XF m msg 5"), IDLE, settle(300), D("reading:selectedUnread"), settle(3000), drained(),
    R("clearMessage"), cmd("markAllRead"), settle(1000), {"op": "search", "text": "is:unread"}, IDLE, settle(600), D("reading:messageCount"),
    {"op": "search", "text": ""}, IDLE, drained(),
    msg("XF m msg 1"), IDLE, R("deleteSelected"), settle(600), drained(), IDLE, settle(1500),
    mbox("m", "trash"), IDLE, waitR("hasSubject:XF m msg 1", "true"), msg("XF m msg 1"), IDLE, cmd("archive"), settle(1500), capall("archive-from-trash"),
    D("reading:messageCount", "reading:errorText", "reading:pendingDelete"), drained(), IDLE, settle(1500),
    mbox("m", "archive"), IDLE, settle(2500), D("reading:messageCount", "reading:subjects"),
    R("reSyncAll"), settle(6000), IDLE, mbox("m", "archive"), IDLE, settle(1500), D("reading:messageCount", "reading:subjects"), cap("archive-after-resync"),
    mbox("n", "inbox"), IDLE, reg("sheet:emptyro"), cmd("emptyTrash"), settle(1200), capall("empty-trash-readonly"), D("reading:errorText"),
    R("selectUnified", text="inbox"), IDLE, cmd("emptyTrash"), settle(1200), capall("empty-trash-unified"), D("reading:errorText"),
], timeout=300000)
