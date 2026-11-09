# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xb pass 2g (scratch, not in scripts/ui-scenarios): the same draft opened in two windows. Usage: python3 gen11.py"""
import h, os
from h import *
h.OUT=os.path.join(os.path.dirname(os.path.abspath(__file__)),"scratch")
s="xb twice {run}"
opn={"op":"ext","id":"openFirstDraft","email":"alice@local.test"}
sc("xb-draft-opened-twice", ["drafts","a draft opened in two windows (clicked twice in the sidebar) is sent once; the second window cannot send it again or raise an error"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","bob@local.test"), fld("subject",s), body("Original text."), pause(300), close, waitx("composeWindows","0"), pause(1000),
  waitx("drafts:alice@local.test",s), opn, waitx("composeWindows","1"), W, opn, pause(1500), W, cap("two-windows"),
  {"op":"ext","id":"appendBody","window":"@compose","text":" Sent from window one."}, pause(300), send, waitx("pendingCount","0",30000), pause(2500), W, cap("after-first-send"),
  ex([("ext:composeWindows","1")]),
  {"op":"ext","id":"appendBody","window":"@compose","text":" Sent from window two."}, pause(300), send, pause(5000), W, cap("after-second-send"),
  {"op":"assert","expect":[{"model":"errorText","equals":""}]}]
 +add("bob")+[mbox("bob"), msg("="+s), W, pause(4000), cap("bob-inbox",["main"]), ex([("ext:messageCount:"+s,"1")])]+Q, 300000)
