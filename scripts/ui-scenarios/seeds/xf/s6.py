# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf UIT-1 undo scenarios. Usage (from anywhere): python3 scripts/ui-scenarios/seeds/xf/s6.py
Reseed first: python3 scripts/ui-scenarios/seeds/xf/seed.py u   (xf-u@local.test gets "XF u msg 1..5").
Writes xf-undo-after-archive / -after-delete / -banner / xf-move-to-folder-undo into scripts/ui-scenarios/."""
import os, gen
gen.ROOT = os.path.abspath(os.path.dirname(os.path.abspath(__file__)) + "/../../../..")  # repo root (gen.py's own ROOT is stale)
from gen import *
def lab(step, text): step["label"] = text; return step
def EL(text, *pairs): return lab(E(*pairs), text)
def waitX(key, val, t=40000): return X("waitExt", name=key, text=val, timeoutMs=t)
SETUP = [add("u"), writes("u"), mbox("u", "inbox"), IDLE, msg("XF u msg 3"), IDLE, settle(2500), drained()]
BACK = [settle(2500), drained(), IDLE, settle(1500)]

# Archive, then undo. The harness window is never the app's main window, so a synthetic Cmd-Z cannot reach
# Edit > Undo (the menu item reads "Undo", winUndo reads "Undo Archive"); the registration is unit-tested
# (AppModelUndoRemovalTests.testUndoManagerActionNameAndSingleFire) and Cmd-Z is a step of Marc's GUI check.
# The command below is what the menu/`z` route to.
write("xf-undo-after-archive", ["undo", "archive", "Cmd-Z"], SETUP + [
    cmd("archive"), settle(800),
    EL("archived: the row left and an undo entry exists", ("reading:messageCount", "4"), ("reading:undoDepth", "1")),
    EL("the window's UndoManager holds 'Undo Archive' (what Edit > Undo runs)", ("ext:editUndo", "Undo|true|window|winCanUndo=true|winUndo=Undo Archive")),
    lab(cmd("undoRemoval"), "undo the archive (menu command / bare z route)"),
] + BACK + [cap("after-undo"),
    EL("Undo after Archive brings the message back to the Inbox",
       ("reading:messageCount", "5"), ("reading:hasSubject:XF u msg 3", "true"), ("reading:undoDepth", "0")),
], timeout=300000)

# Delete, then the bare `z` key (same notification the BareKeyMonitor posts).
write("xf-undo-after-delete", ["undo", "delete", "bare z"], SETUP + [
    cmd("delete"), settle(800),
    EL("deleted: the row left and an undo entry exists", ("reading:messageCount", "4"), ("reading:undoDepth", "1")),
    lab(cmd("undoRemoval"), "bare z"),
] + BACK + [cap("after-undo"),
    EL("Undo after Delete brings the message back to the Inbox",
       ("reading:messageCount", "5"), ("reading:hasSubject:XF u msg 3", "true"), ("reading:undoDepth", "0")),
], timeout=300000)

# The banner: shown after Archive, "Returning…" after Undo, gone once the message is back.
write("xf-undo-banner", ["undo", "banner"], SETUP + [
    cmd("archive"), settle(800), cap("banner-offered"),
    EL("the banner offers Undo", ("ext:removalBanner", "offering"), ("ext:removalBannerText", "Archived 1 message")),
    cmd("undoRemoval"), settle(300),
    EL("Undo switches it to Returning", ("ext:removalBanner", "returning")),
    cap("banner-returning"),
    waitX("removalBanner", "hidden", 45000), IDLE, settle(1000),
    EL("the message is back and the banner is gone", ("reading:messageCount", "5"), ("ext:removalBanner", "hidden")),
], timeout=300000)

# Move to Folder (the picker itself needs Marc's eyes; this drives the model call it makes), then undo.
write("xf-move-to-folder-undo", ["undo", "move to folder"], SETUP + [
    X("moveSelectedTo", text="Trash", allowServerWrites=True), settle(800),
    EL("moved: the row left and a Move undo entry exists", ("reading:messageCount", "4"), ("reading:undoDepth", "1"),
       ("ext:removalBannerText", "Moved 1 message")),
    cmd("undoRemoval"),
] + BACK + [cap("after-undo"),
    EL("Undo after Move brings the message back to the Inbox",
       ("reading:messageCount", "5"), ("reading:hasSubject:XF u msg 3", "true")),
], timeout=300000)
