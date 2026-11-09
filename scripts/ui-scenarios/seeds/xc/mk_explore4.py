# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from gen import *
from mk_all import P, W, subj, body, send, sent, wr, other
st = wr()
st += sel("XC plain note") + reply("reply") + subj("Re: XC plain note draft {run}") + body("Half-written reply {run}\n\nOn 5 Oct 2026 at 13:48, Anna Puig wrote:\n> Hi,") + close()
st += [{"op": "ext", "id": "openFirstDraft", "email": ME}, {"op": "ext", "id": "waitExt", "name": "composeWindows", "text": "1"}, {"op": "wait", "until": "idle"}] + cap("reopened")
st += expect(("ext:bodyHas:Half-written reply", "true"), ("ext:drafts:" + ME, "?")) + send() + sent()
st += other("xc-anna@local.test", "anna") + [{"op": "ext", "id": "selectMessage", "text": "Re: XC plain note draft {run}"}, {"op": "wait", "until": "idle"}, {"op": "ext", "id": "loadSource"}]
st += expect(("ext:src.has:In-Reply-To: ", "true"), ("ext:src.has:> Hi,", "true"))
write("xc-explore4", ["explore"], st, timeout=240000)
