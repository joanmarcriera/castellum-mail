# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xb pass 2f: Send Now on a scheduled send (fixed subject: the harness scheduleSend op does not substitute {run}). Usage: python3 gen10.py"""
from h import *
OB="title:Outbox"
ob=[{"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:"+OB},W]
sc("xb-send-later-send-now", ["send later","Send Now on a scheduled send empties the Outbox and the message arrives (delivery count checked outside with imapcount.py: the subject is not per-run)"],
 start+add("alice",True)+[{"op":"reading","id":"scheduleSend","accountRef":"alice","text":"xb sendnow fixed"}, W, ex([("ext:pendingCount","1")])]+ob+[
  {"op":"accounts","id":"click","window":OB,"text":"383,50"}, waitx("pendingCount","0",30000), pause(800), W, cap("after-send-now"),
  mbox("alice"), msg("=xb sendnow fixed",60000), W, cap("delivered",["main"])]+Q, 180000)
