# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xb pass 2c: fixes of pass-2 assertions + markdown signature defect + send later + pasted recipients + close-with-unsent. Usage: python3 gen7.py"""
from h import *
OB="title:Outbox"
ob=[{"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:"+OB},W]
sent=[waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
def recv(p, subj): return add(p)+[mbox(p), msg("="+subj), W, src]
nodrafts={"op":"assert","window":"main","expect":[{"find":{"text":"Local Drafts"},"exists":False}]}

s="xb undo {run}"
sc("xb-undo-send-reopen-resend", ["undo send","Undo reopens the draft intact; sending it again delivers exactly once and leaves no draft in the sidebar"],
 start+[pref("undoSendDelay","5")]+add("alice",True)+compose()+[fld("to","bob@local.test"), fld("cc","carol@local.test"), fld("subject",s), body("First version."),
  send, waitx("undoBanner","shown",15000), W, cap("banner",["main"]), ex([("ext:composeWindows","0"),("ext:pendingCount","1")]),
  {"op":"ext","id":"keyEquivalent","window":"main","text":"cmd+z"}, waitx("undoBanner","hidden"), waitx("composeWindows","1"), W, cap("reopened"),
  ex([("ext:body","First version."),("ext:pendingCount","0")]), waitx("subject",s),
  {"op":"ext","id":"appendBody","window":"@compose","text":" Second version."}, pause(300),
  send, waitx("undoBanner","shown",15000), waitx("undoBanner","hidden",20000), waitx("pendingCount","0",30000), pause(1500), W, cap("after-send",["main"]), nodrafts]
 +recv("bob",s)+[cap("bob-got-it",["main"]),
  ex([("ext:src.has:First version. Second version.","true"),("ext:src.has:carol@local.test","true")]),
  waitx("messageCount:"+s,"1")]+Q, 300000)

s="xb eval then on {run}"
sc("xb-eval-then-enable-send", ["write safety","Send is refused in evaluation mode with the enable-writes hint; after enabling writes the same window sends once"],
 start+[pref("undoSendDelay","0")]+add("alice")+compose()+[fld("to","bob@local.test"), fld("subject",s), body("Only after writes are on."),
  send, pause(1500), cap("refused"), ex([("ext:composeWindows","1"),("ext:pendingCount","0")]),
  {"op":"ext","id":"enableWrites","email":"alice@local.test"}, pause(500), send]+sent+[pause(1000), W, cap("after-send",["main"]), nodrafts]
 +recv("bob",s)+[waitx("messageCount:"+s,"1")]+Q, 240000)

s="xb bcc {run}"
sc("xb-bcc-not-leaked", ["compose","Bcc recipient gets the message but is not named in the copy To/Cc recipients receive; a quoted name with a comma stays one address"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","\"Soler, Dave\" <dave@local.test>"), fld("cc","Bob Ferrer <bob@local.test>"), fld("bcc","carol@local.test"),
  fld("subject",s), body("Carol is a hidden recipient."), cap("typed"), send]+sent
 +recv("dave",s)+[pause(1000), cap("dave-reads",["main"]), ex([("ext:src.has:carol@local.test","false"),("ext:src.count:dave@local.test","1"),("ext:src.has:\"Bob Ferrer\" <bob@local.test>","true")]),
  {"op":"assert","window":"main","expect":[{"find":{"text":"Soler, Dave"},"exists":True}]}]
 +recv("carol",s)+[cap("carol-reads",["main"]), ex([("ext:src.has:Carol is a hidden recipient.","true")])]+Q, 300000)

s="xb md sig {run}"
sc("xb-defect-markdown-signature-lines-merged", ["markdown compose","with Markdown on, a multi-line signature must keep its line breaks in the HTML part the recipient reads (asserts a <br> after the first signature line)"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+[pref("signature:alice@local.test","-- \nAlice Puig\nCastellum tester\n+34 600 000 000")]+compose()+[
  fld("to","bob@local.test"), fld("subject",s), {"op":"ext","id":"composeSet","window":"@compose","name":"markdown","value":True}, pause(800), W, cap("compose-preview"),
  send]+sent+recv("bob",s)+[pause(1500), cap("bob-reads",["main"]),
  ex([("ext:src.has:Alice Puig<br","true")])]+Q, 240000)

s="xb later {run}"
sc("xb-send-later-outbox", ["send later","Send in One Hour closes the window and lists the message in the Outbox with its time; nothing is delivered"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","bob@local.test"), fld("subject",s), body("Not before an hour from now."),
  {"op":"command","id":"sendLaterInOneHour","allowServerWrites":True}, waitx("composeWindows","0",15000), waitx("pendingCount","1",10000), pause(1000), W, cap("after-schedule",["main"])]
 +ob+[cap("outbox-scheduled"),
  {"op":"assert","window":OB,"rules":["nonBlank","onScreen"],"expect":[{"find":{"text":"xb later"},"exists":True}]}]+Q, 240000)

s="xb paste {run}"
sc("xb-pasted-recipient-list", ["compose","a recipient list pasted with newlines or spaces instead of commas is either accepted or named as invalid; nothing is sent to a partial list"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","bob@local.test\ncarol@local.test"), fld("subject",s), body("Pasted list."), send, pause(1000), cap("newline-list"),
  ex([("ext:composeWindows","1"),("ext:pendingCount","0")]),
  fld("to","bob@local.test carol@local.test"), send, pause(1000), cap("space-list"), ex([("ext:composeWindows","1"),("ext:pendingCount","0")]),
  fld("to","<bob@local.test>, carol@local.test,"), send]+sent+recv("carol",s)+[ex([("ext:src.has:bob@local.test","true")])]+Q, 240000)

subj="xb unsent roundtrip"
T="title:"+subj
sc("xb-close-unsent-saves-draft", ["drafts","closing a window with unsent text keeps a draft; reopening restores To, Cc, Bcc, subject and body; closing it again makes no second draft"],
 start+add("alice")+compose()+[fld("to","bob@local.test, Carol Vidal <carol@local.test>"), fld("cc","\"Soler, Dave\" <dave@local.test>"), fld("bcc","alice@local.test"),
  fld("subject",subj), body("Line one.\n\nLine three with àccents 🙂."), pause(300), cap("typed"), close, waitx("composeWindows","0"), pause(1000), W,
  waitx("drafts:alice@local.test",subj), cap("after-close",["main"]),
  {"op":"assert","window":"main","expect":[{"find":{"text":"Local Drafts"},"exists":True},{"find":{"text":subj},"exists":True}]},
  {"op":"ext","id":"openFirstDraft","email":"alice@local.test"}, waitx("composeWindows","1"), W, cap("reopened"),
  ex([("ext:body","Line one.\n\nLine three with àccents 🙂.")]),
  {"op":"assert","window":T,"expect":[{"find":{"text":"Carol Vidal <carol@local.test>"},"exists":True},{"find":{"text":"dave@local.test"},"exists":True},{"find":{"text":"Soler, Dave"},"exists":True},{"find":{"text":"alice@local.test"},"exists":True}]},
  close, waitx("composeWindows","0"), pause(1000), waitx("drafts:alice@local.test",subj)]+Q, 180000)
