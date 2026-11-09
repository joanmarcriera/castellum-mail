# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from h import *
OB="title:Outbox"
ob=[{"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:"+OB},W]
fail_send=start+[pref("undoSendDelay","0")]+add("alice",True,smtp="127.0.0.1:1")+compose()+[
  fld("to","bob@local.test"), fld("subject","xb outbox {run}"), body("A message the SMTP server never accepts."), send,
  waitx("pendingCount","1",30000), pause(1500), W]
sc("xb-outbox-failed-send-kept", ["outbox","an SMTP failure keeps the message in the Outbox, says so in the compose window, and Retry keeps it"],
 fail_send+[cap("send-failed"),
  {"op":"assert","window":"compose","expect":[{"find":{"text":"kept in Outbox"},"exists":True},{"find":{"text":"127.0.0.1:1"},"exists":True}]},
  ex([("ext:composeWindows","1"),("ext:pendingCount","1")]),
  close, waitx("composeWindows","0")]+ob+[cap("outbox-failed"),
  {"op":"assert","window":OB,"rules":["nonBlank","onScreen"]},
  {"op":"accounts","id":"click","window":OB,"text":"477,50"}, pause(4000), W, cap("after-retry"),
  ex([("ext:pendingCount","1")]), waitx("drafts:alice@local.test","xb outbox {run}")]+Q)

sc("xb-defect-outbox-cancel-strands-message", ["outbox","Cancel on a failed send must give the message back as an editable draft (or delete it), not leave it locked"],
 fail_send+[close, waitx("composeWindows","0")]+ob+[cap("outbox-failed"),
  {"op":"accounts","id":"click","window":OB,"text":"522,50"}, waitx("pendingCount","0",10000), pause(800), W, cap("after-cancel"),
  waitx("drafts:alice@local.test","xb outbox {run}"),
  {"op":"closeWindow","window":OB},
  {"op":"ext","id":"openFirstDraft","email":"alice@local.test"}, waitx("composeWindows","1"), W,
  {"op":"ext","id":"appendBody","window":"@compose","text":"\nEdited after cancelling the send."}, pause(300), cap("cancelled-message-reopened"),
  close, waitx("composeWindows","0"), pause(1500), W, cap("after-edit-close",["main"]),
  {"op":"assert","expect":[{"model":"errorText","equals":""}]},
  {"op":"ext","id":"openFirstDraft","email":"alice@local.test"}, waitx("composeWindows","1"), W,
  ex([("ext:bodyHas:Edited after cancelling the send.","true")])]+Q)
