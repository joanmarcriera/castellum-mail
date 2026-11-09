# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from gen import *
P = lambda ms: [{"op": "ext", "id": "pause", "text": str(ms)}]
st = head() + sel("XC plain note") + reply("reply")
st += [{"op": "ext", "id": "setBody", "window": "@compose", "text": "XC-NOWRITE-{run} see you Friday"}] + P(15000) + cap("ro-typed")
st += [{"op": "ext", "id": "keyEquivalent", "window": "@compose", "text": "cmd+return"}] + P(4000) + cap("ro-send")
st += expect(("ext:composeWindows", "1"), ("ext:pendingCount", "0"))
write("xc-explore2", ["explore: read-only reply"], st)
