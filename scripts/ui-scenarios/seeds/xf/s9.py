# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf UIT-1 plain-text folding scenario. Usage: python3 scripts/ui-scenarios/seeds/xf/seed_fold.py && python3 scripts/ui-scenarios/seeds/xf/s9.py
Writes xf-quote-folding.json: a plain-text mail with a 12-line quote and a 6-line signature opens with both
folded; a block can be toggled and View > Expand All Quotes opens everything; another message and back
restores the defaults; the text a reader selects is published for Reply (model-level, no pointer)."""
import os, gen
gen.ROOT = os.path.abspath(os.path.dirname(os.path.abspath(__file__)) + "/../../../..")
from gen import *
def lab(step, text): step["label"] = text; return step
def EL(text, *pairs): return lab(E(*pairs), text)
ADD = {"op": "addAccount", "email": "xf-f@local.test", "password": "xf-f@local.test", "as": "f", "waitFor": "synced", "timeoutMs": 90000}
def opn(subject): return msg(subject)
write("xf-quote-folding", ["plain text", "quote folding", "signature folding"], [
    ADD, mbox("f", "inbox"), IDLE, settle(1500),
    msg("fold other"), IDLE, settle(500),
    EL("a message with nothing long to fold shows everything", ("reading:detailFolded", "0")),
    msg("XF fold"), IDLE, settle(800), cap("folded"),
    EL("four blocks: prose, the 12-line quote, prose, the 6-line signature; quote and signature start folded",
       ("reading:detailBlocks", "4"), ("reading:detailFolded", "2")),
    R("detailToggleFold", text="1"), IDLE, settle(300), cap("quote-open"),
    EL("opening the quote leaves the signature folded", ("reading:detailFolded", "1")),
    R("detailToggleFold", text="1"), settle(200),
    EL("closing it again", ("reading:detailFolded", "2")),
    R("detailExpandAll"), settle(200),
    EL("View > Expand All Quotes opens both", ("reading:detailFolded", "0")),
    R("detailSelect", text="Thanks for the update"), settle(200),
    EL("the selected text is published for Reply", ("reading:readingSelection", "Thanks for the update")),
    msg("fold other"), IDLE, settle(500),
    EL("another message: no carried-over selection", ("reading:readingSelection", "")),
    msg("XF fold"), IDLE, settle(800),
    EL("back on the first message the folds are back to the default", ("reading:detailFolded", "2")),
], timeout=240000)

# Slice 4: Reply quotes the selection; with none it quotes the whole message minus the sender's signature.
write("xf-reply-selection", ["reply", "selection", "signature"], [
    ADD, mbox("f", "inbox"), IDLE, settle(1500),
    msg("XF fold"), IDLE, settle(800),
    R("detailSelect", text="Thanks for the update"), settle(200),
    {"op": "command", "id": "reply"}, {"op": "wait", "until": "window:title:Re: XF fold", "timeoutMs": 15000}, IDLE, settle(500),
    EL("a selection is all that is quoted",
       ("ext:bodyHas:> Thanks for the update", "true"), ("ext:bodyHas:See you soon.", "false"), ("ext:bodyHas:quoted line", "false")),
    {"op": "capture", "name": "reply-with-selection", "windows": ["title:Re: XF fold"]},
    X("closeCompose"), IDLE, settle(500),
    msg("fold other"), IDLE, settle(500), msg("XF fold"), IDLE, settle(800),
    EL("the selection did not follow to the other message and back", ("reading:readingSelection", "")),
    {"op": "command", "id": "reply"}, {"op": "wait", "until": "window:title:Re: XF fold", "timeoutMs": 15000}, IDLE, settle(500),
    EL("no selection: the whole message is quoted, without the sender's signature",
       ("ext:bodyHas:> Thanks for the update", "true"), ("ext:bodyHas:> See you soon.", "true"),
       ("ext:bodyHas:Cambridge", "false"), ("ext:bodyHas:Marc Riera", "false")),
    {"op": "capture", "name": "reply-whole", "windows": ["title:Re: XF fold"]},
], timeout=240000)
