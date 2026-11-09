# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xb pass 2e: corrected markdown-signature defect, close-unsent, outbox scheduled actions, two sends in one undo window, properly named duplicate-recipient defect. Usage: python3 gen9.py"""
from h import *
OB="title:Outbox"
ob=[{"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:"+OB},W]
sent=[waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
def recv(p, subj): return add(p)+[mbox(p), msg("="+subj), W, src]
def click(xy): return {"op":"accounts","id":"click","window":OB,"text":xy}

s="xb md sig {run}"
sc("xb-defect-markdown-signature-lines-merged", ["markdown compose","with Markdown on, a multi-line signature must keep its line breaks in the HTML part the recipient reads (asserts a <br> after the first signature line; today the lines are one paragraph)"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+[pref("signature:alice@local.test","Alice Puig\nCastellum tester\n+34 600 000 000")]+compose()+[
  fld("to","bob@local.test"), fld("subject",s), {"op":"ext","id":"composeSet","window":"@compose","name":"markdown","value":True}, pause(800), W, cap("compose-preview"),
  send]+sent+recv("bob",s)+[pause(1500), cap("bob-reads",["main"]),
  ex([("ext:src.has:Castellum tester","true"),("ext:src.has:Alice Puig<br","true")])]+Q, 240000)

s="xb unsent {run}"
sc("xb-close-unsent-saves-draft", ["drafts","closing a window with unsent text keeps one draft; reopened and sent, it carries the To, Cc, Bcc, subject and body that were typed"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","bob@local.test, Carol Vidal <carol@local.test>"), fld("cc","\"Soler, Dave\" <dave@local.test>"), fld("bcc","alice@local.test"),
  fld("subject",s), body("Line one.\n\nLine three with àccents 🙂."), pause(300), cap("typed"), close, waitx("composeWindows","0"), pause(1000), W,
  waitx("drafts:alice@local.test",s), ex([("ext:pendingCount","0")]),
  {"op":"ext","id":"openFirstDraft","email":"alice@local.test"}, waitx("composeWindows","1"), W, cap("reopened"),
  ex([("ext:body","Line one.\n\nLine three with àccents 🙂.")]), waitx("subject",s),
  close, waitx("composeWindows","0"), pause(1000), waitx("drafts:alice@local.test",s),
  {"op":"ext","id":"openFirstDraft","email":"alice@local.test"}, waitx("composeWindows","1"), W, send]+sent
 +recv("dave",s)+[pause(1000), cap("dave-reads",["main"]),
  ex([("ext:src.has:\"Carol Vidal\" <carol@local.test>","true"),("ext:src.has:bob@local.test","true"),("ext:src.has:Bcc:","false"),("ext:src.has:Line three with","true")]),
  mbox("alice"), msg("="+s), W, cap("alice-bcc-copy",["main"])]+Q, 300000)

sched=start+add("alice",True)+[{"op":"reading","id":"scheduleSend","accountRef":"alice","text":"xb scheduled {run}"}, W, ex([("ext:pendingCount","1")])]+ob
sc("xb-send-later-outbox", ["send later","a scheduled send is listed in the Outbox with its time and actions, and is not delivered"],
 sched+[cap("outbox-scheduled"), {"op":"assert","window":"outbox","rules":["nonBlank","onScreen","noClippedText"]}, ex([("ext:pendingCount","1")])]+Q, 180000)

sc("xb-send-later-cancel-back-to-drafts", ["send later","Cancel on a scheduled send returns it to Drafts, where it can be edited and saved again without an error"],
 sched+[click("522,50"), waitx("pendingCount","0",10000), pause(800), W, cap("after-cancel"),
  {"op":"ext","id":"openFirstDraft","email":"alice@local.test"}, waitx("composeWindows","1"), W,
  {"op":"ext","id":"appendBody","window":"@compose","text":"Edited after cancelling the schedule."}, pause(300), cap("reopened-edited"),
  close, waitx("composeWindows","0"), pause(1500), W, {"op":"assert","expect":[{"model":"errorText","equals":""}]},
  {"op":"ext","id":"openFirstDraft","email":"alice@local.test"}, waitx("composeWindows","1"), W,
  ex([("ext:bodyHas:Edited after cancelling the schedule.","true"),("ext:pendingCount","0")])]+Q, 180000)

sc("xb-send-later-send-now", ["send later","Send Now on a scheduled send delivers it once and empties the Outbox"],
 sched+[click("383,50"), waitx("pendingCount","0",30000), pause(800), W, cap("after-send-now"),
  mbox("alice"), msg("=xb scheduled {run}"), W, cap("delivered",["main"]), waitx("messageCount:xb scheduled {run}","1")]+Q, 180000)

a,b="xb first of two {run}","xb second of two {run}"
sc("xb-undo-second-of-two-sends", ["undo send","two sends inside one undo window: undoing the second reopens it and the first is still delivered exactly once"],
 start+[pref("undoSendDelay","10")]+add("alice",True)+compose()+[fld("to","bob@local.test"), fld("subject",a), body("First."), send, waitx("composeWindows","0",15000), waitx("undoBanner","shown",15000)]
 +compose()+[fld("to","bob@local.test"), fld("subject",b), body("Second."), send, waitx("composeWindows","0",15000), waitx("pendingCount","2",5000), cap("two-queued",["main"]),
  {"op":"ext","id":"keyEquivalent","window":"main","text":"cmd+z"}, waitx("composeWindows","1"), waitx("subject",b), W, cap("second-reopened"),
  waitx("pendingCount","0",30000), close, waitx("composeWindows","0")]
 +add("bob")+[mbox("bob"), msg("="+a), W, cap("bob-inbox",["main"]), pause(3000), ex([("ext:messageCount:"+a,"1"),("ext:messageCount:"+b,"0")])]+Q, 300000)

sc("xb-defect-duplicate-recipient-delivered-twice", ["compose","the same address typed more than once (To twice, a third time with a name and other case, and in Cc) is delivered once and named once in the headers"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[
  fld("to","bob@local.test, bob@local.test, Bob Ferrer <BOB@local.test>"), fld("cc","bob@local.test"),
  fld("subject","xb dup2 {run}"), body("Once is enough."), cap("dup-typed"), send]+sent
 +add("bob")+[mbox("bob"), msg("=xb dup2 {run}"), W, pause(3000), cap("bob-inbox",["main"]), src,
  ex([("ext:messageCount:xb dup2 {run}","1")])]+Q, 240000)

fail_send=start+[pref("undoSendDelay","0")]+add("alice",True,smtp="127.0.0.1:1")+compose()+[
  fld("to","bob@local.test"), fld("subject","xb outbox2 {run}"), body("A message the SMTP server never accepts."), send,
  waitx("pendingCount","1",30000), pause(1500), W]
sc("xb-outbox-failed-send-stays-queued", ["outbox","an SMTP failure keeps the message queued, the compose window names the SMTP endpoint, and the Outbox lists the failed send"],
 fail_send+[cap("send-failed"),
  {"op":"assert","window":"compose","expect":[{"find":{"text":"kept in Outbox"},"exists":True},{"find":{"text":"127.0.0.1:1"},"exists":True}]},
  ex([("ext:composeWindows","1"),("ext:pendingCount","1")]),
  close, waitx("composeWindows","0")]+ob+[cap("outbox-failed"),
  {"op":"assert","window":"outbox","rules":["nonBlank","onScreen"]}, ex([("ext:pendingCount","1")])]+Q)
