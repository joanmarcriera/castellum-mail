# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer, session 2 probe 4: delete/archive while the server is unreachable; rapid double flag. Usage: python3 p4.py"""
from p2 import *
def ep(ref, hp, sync=True): return A("setEndpoint", accountRef=ref, text=hp, value=sync)
wprobe("xf-tmp-p4", [
    add("o"), writes("o"), mbox("o", "inbox"), IDLE,
    msg("XF o msg 3"), IDLE, settle(2500), drained(), cmd("toggleFlag"), cmd("toggleFlag"), settle(600), cmd("toggleFlag"), settle(800), drained(),
    D("reading:cell:flag:0"),
    ep("o", "127.0.0.1:3999"), settle(1500),
    msg("XF o msg 4"), IDLE, settle(500), R("deleteSelected"), settle(1500), cap("deleted-offline"),
    D("reading:messageCount", "ext:pendingCount", "reading:errorText", "reading:syncBanner"),
    msg("XF o msg 2"), IDLE, cmd("archive"), settle(1500),
    R("reSyncAll"), settle(6000), capall("offline-after-resync"),
    D("reading:messageCount", "reading:subjects", "ext:pendingCount", "reading:errorText", "reading:syncBanner"),
    mbox("o", "inbox"), IDLE, settle(800), D("reading:messageCount", "reading:subjects"),
    ep("o", "127.0.0.1:3143"), settle(1000), R("reSyncAll"), settle(4000), X("waitExt", name="pendingCount", text="0", timeoutMs=60000), IDLE, settle(2000),
    mbox("o", "inbox"), IDLE, settle(800), cap("back-online"),
    D("reading:messageCount", "reading:subjects", "ext:pendingCount", "reading:errorText", "reading:syncBanner"),
], timeout=300000)
