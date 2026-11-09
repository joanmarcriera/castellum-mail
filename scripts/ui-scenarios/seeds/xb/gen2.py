# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from h import *
OB="title:Outbox"
ob=[{"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:"+OB},W]
sc("xb-explore-outbox", ["explore"],
 start+[pref("undoSendDelay","0")]+add("alice",True,smtp="127.0.0.1:1")+compose()+[
  fld("to","bob@local.test"), fld("subject","xb outbox first {run}"), body("First failing message."), send, pause(4000), W,
  close, waitx("composeWindows","0")]
 +ob+[cap("outbox-failed",[OB]),
  {"op":"accounts","id":"click","window":OB,"text":"522,50"}, pause(1500), W, cap("after-cancel-click"),
  ex([("ext:pendingCount","0"),("ext:drafts:alice@local.test","x")]),
  {"op":"ext","id":"describe","window":OB}]+Q)
