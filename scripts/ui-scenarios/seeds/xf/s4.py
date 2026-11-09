# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer, session 2 (third file): Mark All as Read queued offline, mail arrives meanwhile. Usage: python3 s4.py
The scenario needs build/explore/xf/late-mail.sh to deliver the late message while the app is 'offline'."""
from gen import *
def lab(step, text): step["label"] = text; return step
def EL(text, *pairs): return lab(E(*pairs), text)
def ep(ref, hp): return A("setEndpoint", accountRef=ref, text=hp)
write("xf-defect-mark-all-read-replay-marks-new-mail", ["mark-all-read", "offline", "replay", "read"], [
    add("t"), writes("t"), mbox("t", "inbox"), IDLE, EL("5 seeded, all unread", ("reading:messageCount", "5")),
    ep("t", "127.0.0.1:3999"), settle(1500), R("clearMessage"), cmd("markAllRead"), settle(800), cap("marked-all-while-offline"),
    lab(settle(15000), "RUN VIA build/explore/xf/late-mail.sh: it delivers 'XF t late arrival' by SMTP now, while the app cannot reach the server"),
    ep("t", "127.0.0.1:3143"), settle(1000), R("reSyncAll"), settle(3000), drained(60000), IDLE, R("reSyncAll"), settle(4000), IDLE,
    mbox("t", "inbox"), IDLE, waitR("hasSubject:XF t late arrival", "true", 30000), settle(1500), cap("back-online-late-mail-listed"),
    {"op": "search", "text": "is:unread"}, IDLE, settle(700),
    EL("mail that arrived AFTER Mark All as Read was pressed must still be unread once the queue replays", ("reading:hasSubject:XF t late arrival", "true")),
], timeout=300000)
