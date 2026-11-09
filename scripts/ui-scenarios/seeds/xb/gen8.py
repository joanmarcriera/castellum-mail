# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xb pass 2d: drops assertions the harness cannot see (sidebar text, field values), close-unsent now proves the fields by sending the reopened draft. Usage: python3 gen8.py"""
from h import *
OB="title:Outbox"
ob=[{"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:"+OB},W]
sent=[waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
def recv(p, subj): return add(p)+[mbox(p), msg("="+subj), W, src]

s="xb undo {run}"
sc("xb-undo-send-reopen-resend", ["undo send","Undo reopens the draft intact (nothing queued); sending it again delivers exactly once with the edit"],
 start+[pref("undoSendDelay","5")]+add("alice",True)+compose()+[fld("to","bob@local.test"), fld("cc","carol@local.test"), fld("subject",s), body("First version."),
  send, waitx("undoBanner","shown",15000), W, cap("banner",["main"]), ex([("ext:composeWindows","0"),("ext:pendingCount","1")]),
  {"op":"ext","id":"keyEquivalent","window":"main","text":"cmd+z"}, waitx("undoBanner","hidden"), waitx("composeWindows","1"), W, cap("reopened"),
  ex([("ext:body","First version."),("ext:pendingCount","0")]), waitx("subject",s),
  {"op":"ext","id":"appendBody","window":"@compose","text":" Second version."}, pause(300),
  send, waitx("undoBanner","shown",15000), waitx("undoBanner","hidden",20000), waitx("pendingCount","0",30000), pause(1500), W, cap("after-send",["main"])]
 +recv("bob",s)+[cap("bob-got-it",["main"]),
  ex([("ext:src.has:First version. Second version.","true"),("ext:src.has:carol@local.test","true")]),
  waitx("messageCount:"+s,"1")]+Q, 300000)

s="xb eval then on {run}"
sc("xb-eval-then-enable-send", ["write safety","Send is refused in evaluation mode with the enable-writes hint and nothing is queued; after enabling writes the same window sends once"],
 start+[pref("undoSendDelay","0")]+add("alice")+compose()+[fld("to","bob@local.test"), fld("subject",s), body("Only after writes are on."),
  send, pause(1500), cap("refused"), ex([("ext:composeWindows","1"),("ext:pendingCount","0"),("ext:undoBanner","hidden")]),
  {"op":"ext","id":"enableWrites","email":"alice@local.test"}, pause(500), send]+sent+[pause(1000), W, cap("after-send",["main"])]
 +recv("bob",s)+[waitx("messageCount:"+s,"1")]+Q, 240000)

s="xb bcc {run}"
sc("xb-bcc-not-leaked", ["compose","Bcc recipient gets the message but is not named in the copy To/Cc recipients receive; a quoted name with a comma stays one address"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","\"Soler, Dave\" <dave@local.test>"), fld("cc","Bob Ferrer <bob@local.test>"), fld("bcc","carol@local.test"),
  fld("subject",s), body("Carol is a hidden recipient."), cap("typed"), send]+sent
 +recv("dave",s)+[pause(1000), cap("dave-reads",["main"]), ex([("ext:src.has:carol@local.test","false"),("ext:src.count:dave@local.test","1"),("ext:src.has:\"Bob Ferrer\" <bob@local.test>","true")])]
 +recv("carol",s)+[cap("carol-reads",["main"]), ex([("ext:src.has:Carol is a hidden recipient.","true")])]+Q, 300000)

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
  ex([("ext:src.has:\"Carol Vidal\" <carol@local.test>","true"),("ext:src.has:bob@local.test","true"),("ext:src.has:alice@local.test>","false"),("ext:src.has:Line three with","true")]),
  mbox("alice"), msg("="+s), W, cap("alice-bcc-copy",["main"])]+Q, 300000)

sc("xb-send-later-outbox", ["send later","a scheduled send is listed in the Outbox with its time and actions, and is not delivered"],
 start+add("alice",True)+[{"op":"reading","id":"scheduleSend","accountRef":"alice","text":"xb scheduled {run}"}, W, ex([("ext:pendingCount","1")])]
 +ob+[cap("outbox-scheduled"), {"op":"assert","window":OB,"rules":["nonBlank","onScreen","noClippedText"]}]+Q, 180000)
