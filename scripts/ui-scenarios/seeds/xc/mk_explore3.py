# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from gen import *
P = lambda ms: [{"op": "ext", "id": "pause", "text": str(ms)}]
st = head(writes=True) + [{"op": "ext", "id": "setPref", "name": "undoSendDelay", "text": "0"}]
st += sel("XC plain note") + reply("reply")
st += [{"op": "ext", "id": "setField", "window": "@compose", "name": "subject", "text": "Re: XC plain note {run}"},
       {"op": "ext", "id": "setBody", "window": "@compose", "text": "Please find attached the agenda."},
       {"op": "ext", "id": "keyEquivalent", "window": "@compose", "text": "cmd+return"}] + P(1500) + cap("warn")
write("xc-explore3", ["explore: forgotten attachment"], st)
