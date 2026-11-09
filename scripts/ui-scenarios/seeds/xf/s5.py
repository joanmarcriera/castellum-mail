# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer, session 2 (fourth file): Undo after Archive / Delete. Usage: python3 s5.py"""
from gen import *
def lab(step, text): step["label"] = text; return step
def EL(text, *pairs): return lab(E(*pairs), text)
write("xf-defect-no-undo-after-archive-delete", ["undo", "archive", "delete"], [
    add("u"), writes("u"), mbox("u", "inbox"), IDLE, msg("XF u msg 3"), IDLE, settle(2500), drained(),
    cmd("archive"), settle(800), EL("archived", ("reading:messageCount", "4")),
    lab(X("keyEquivalent", window="main", text="cmd+z"), "Cmd+Z right after Archive (Edit > Undo)"), settle(2500), drained(), IDLE, settle(1500), cap("after-undo"),
    EL("Undo after Archive brings the message back to the Inbox", ("reading:messageCount", "5"), ("reading:hasSubject:XF u msg 3", "true")),
], timeout=300000)
