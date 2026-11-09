# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer, session 2 probe 5: writes turned off again while a delete is still queued. Usage: python3 p5.py"""
from p2 import *
def ep(ref, hp): return A("setEndpoint", accountRef=ref, text=hp)
wprobe("xf-tmp-p5", [
    add("s"), writes("s"), mbox("s", "inbox"), IDLE,
    msg("XF s msg 1"), R("deleteSelected"), settle(600), drained(), IDLE, settle(1500),
    ep("s", "127.0.0.1:3999"), settle(1500),
    msg("XF s msg 4"), R("deleteSelected"), settle(1000), D("ext:pendingCount", "reading:messageCount"),
    R("setWrite", accountRef="s", value=False), settle(1000),
    ep("s", "127.0.0.1:3143"), settle(1000), R("reSyncAll"), settle(7000), IDLE,
    mbox("s", "trash"), IDLE, settle(1500), D("reading:messageCount", "reading:subjects", "ext:pendingCount"),
    mbox("s", "inbox"), IDLE, settle(1000), cap("readonly-again-inbox"), D("reading:messageCount", "reading:subjects", "reading:errorText", "reading:syncBanner"),
    msg("XF s msg 4"), IDLE, settle(500),
    R("setWrite", accountRef="s", value=True), settle(1500), X("waitExt", name="pendingCount", text="0", timeoutMs=60000), IDLE, settle(2500),
    mbox("s", "inbox"), IDLE, settle(1000), cap("writes-back-inbox"), D("reading:messageCount", "reading:subjects", "reading:errorText"),
    mbox("s", "trash"), IDLE, settle(1500), D("reading:messageCount", "reading:subjects"),
], timeout=300000)
