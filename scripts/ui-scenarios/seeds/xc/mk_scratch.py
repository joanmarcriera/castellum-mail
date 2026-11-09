# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from gen import *
st = head() + sel("XC plain note") + reply("reply") + expect(("ext:bodyHas:wrote:\n> Hi,", "true"), ("ext:bodyHas:> Hi,\n> \n> Can", "true"), ("ext:bodyHas:Anna Puig wrote:", "true"), ("ext:bodyHas:> Hi,\n> ", "true"), ("ext:bodyHas:> Can you confirm the meeting on Friday?", "true"), ("ext:bodyHas:Hi,\n", "true")) + close()
write("xc-scratch", ["scratch"], st)
