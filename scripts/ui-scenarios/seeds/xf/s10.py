# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf UIT-1 Spotlight continuation scenario. Usage: python3 scripts/ui-scenarios/seeds/xf/seed_sort.py && python3 scripts/ui-scenarios/seeds/xf/s10.py
Writes xf-spotlight-open-result.json. Clicking a real Spotlight result cannot be driven here (that needs the
system index and a signed build: Marc's GUI check, S58 step 8); this drives what the click does, the model entry
point `AppModel.openSpotlightResult`, and asserts the outcome: the right mailbox and message open, and a dead
result shows the notice instead of crashing or doing nothing."""
import os, gen
gen.ROOT = os.path.abspath(os.path.dirname(os.path.abspath(__file__)) + "/../../../..")
from gen import *
def lab(step, text): step["label"] = text; return step
def EL(text, *pairs): return lab(E(*pairs), text)
ADD = {"op": "addAccount", "email": "xf-s@local.test", "password": "xf-s@local.test", "as": "s", "waitFor": "synced", "timeoutMs": 90000}
DEAD = "The message is gone"
write("xf-spotlight-open-result", ["spotlight", "continuation", "reveal"], [
    ADD, mbox("s", "inbox"), IDLE, settle(1500),
    msg("Dates"), IDLE, settle(500), R("remember"),
    R("selectUnified", text="inbox"), IDLE, settle(800),
    EL("start from All Inboxes with nothing of ours selected", ("reading:isUnified", "true")),
    R("spotlightOpen", text="remembered"), IDLE, settle(1500), cap("opened"),
    EL("the result opens its message in its own mailbox", ("reading:selectedSubject", "Dates"),
       ("reading:mailboxRole", "inbox"), ("reading:isUnified", "false"), ("reading:smartNotice", "none")),
    R("spotlightOpen", text="id:nope:INBOX:999999"), IDLE, settle(1500), cap("dead-result"),
    EL("a result whose message is gone shows the notice",
       ("reading:smartNotice", "That message is no longer in this mailbox. It may have been moved or deleted.")),
    R("spotlightOpen", text="remembered"), IDLE, settle(1500),
    EL("opening a live result clears the notice again", ("reading:smartNotice", "none"), ("reading:selectedSubject", "Dates")),
], timeout=240000)
