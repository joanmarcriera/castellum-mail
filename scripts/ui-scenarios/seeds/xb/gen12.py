# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xb pass 2h: the same draft opened in two windows (defect). Usage: python3 gen12.py"""
from h import *
s="xb twice {run}"
opn={"op":"ext","id":"openFirstDraft","email":"alice@local.test"}
sc("xb-defect-draft-opened-twice-dead-end", ["drafts","opening a draft that is already open must bring its window forward, not open a second editor on the same draft: after the first window sends, the second can neither send nor save and shows an internal error"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","bob@local.test"), fld("subject",s), body("Original text."), pause(300), close, waitx("composeWindows","0"), pause(1000),
  waitx("drafts:alice@local.test",s), opn, waitx("composeWindows","1"), W, opn, pause(1500), W, cap("two-windows"),
  {"op":"assert","label":"one editor per draft","expect":[{"windowCount":2}]},
  {"op":"ext","id":"appendBody","window":"@compose","text":" Sent from window one."}, pause(300), send, waitx("pendingCount","0",30000), pause(2500), W, cap("after-first-send"),
  {"op":"ext","id":"appendBody","window":"@compose","text":" Typed in window two."}, pause(300), send, pause(4000), W, cap("after-second-send"),
  {"op":"assert","label":"no internal error","expect":[{"model":"errorText","equals":""}]}]+Q, 300000)
