# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from gen import *
st = head()
cases = [("XC plain note", "replyAll", "plain-replyall"), ("XC plain note", "reply", "plain-reply"), ("XC replyto list", "reply", "replyto"),
         ("RE: XC upper prefix", "reply", "upper-re"), ("Re: Re: XC doubled", "reply", "doubled-re"), ("XC html only", "reply", "html-only"),
         ("XC pressupost amb adjunt", "reply", "cat-attach"), ("XC from myself", "replyAll", "from-myself-all"), ("XC from myself", "reply", "from-myself"),
         ("XC quoted name", "replyAll", "quoted-name"), ("XC long quote", "reply", "long")]
for subj, cmd, nm in cases:
    st += sel(subj) + reply(cmd) + cap(nm) + close()
write("xc-explore1", ["explore: reply/reply-all prefill"], st, timeout=240000)
