# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Second xb pass (2026-10-05 resume): undo, markdown+signature, long unicode, account switch, eval->enable, bcc. Usage: python3 gen5.py"""
from h import *
sent=[waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
def recv(p, subj): return add(p)+[mbox(p), msg("="+subj), W, src]

# S1 undo send -> reopen -> resend: delivered exactly once, no draft left behind
s="xb undo {run}"
sc("xb-undo-send-reopen-resend", ["undo send","Undo reopens the draft intact; sending it again delivers exactly once and leaves no draft"],
 start+[pref("undoSendDelay","5")]+add("alice",True)+compose()+[fld("to","bob@local.test"), fld("cc","carol@local.test"), fld("subject",s), body("First version."),
  send, waitx("undoBanner","shown",15000), W, cap("banner",["main"]), ex([("ext:composeWindows","0"),("ext:pendingCount","1")]),
  {"op":"ext","id":"keyEquivalent","window":"main","text":"cmd+z"}, waitx("undoBanner","hidden"), waitx("composeWindows","1"), W, cap("reopened"),
  ex([("ext:body","First version."),("ext:pendingCount","0")]), waitx("subject",s),
  {"op":"ext","id":"appendBody","window":"@compose","text":" Second version."}, pause(300),
  send, waitx("undoBanner","shown",15000), waitx("undoBanner","hidden",20000), waitx("pendingCount","0",30000), pause(1500), W, cap("after-send",["main"]),
  waitx("drafts:alice@local.test","")]
 +recv("bob",s)+[cap("bob-got-it",["main"]),
  ex([("ext:src.has:First version. Second version.","true"),("ext:src.has:carol@local.test","true")]),
  waitx("messageCount:"+s,"1")]+Q, 300000)

# S3 markdown + signature
s="xb markdown {run}"
md="# Heading\n\nSome **bold** text, a [link](https://example.org/x?a=1&b=2) and `code`.\n\n- one\n- two\n\n-- \nAlice Puig\nCastellum tester"
sc("xb-markdown-signature-send", ["markdown compose","Markdown message with signature is sent as multipart/alternative with rendered HTML and the plain source"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+[pref("signature:alice@local.test","-- \nAlice Puig\nCastellum tester")]+compose()+[
  cap("new-with-signature"), ex([("ext:bodyHas:Alice Puig","true"),("ext:bodyCount:Castellum tester","1")]),
  fld("to","bob@local.test"), fld("subject",s), body(md), {"op":"ext","id":"composeSet","window":"@compose","name":"markdown","value":True}, pause(800), W, cap("markdown-on"),
  send]+sent+recv("bob",s)+[pause(1500), cap("bob-reads-markdown",["main"]),
  ex([("ext:src.has:multipart/alternative","true"),("ext:src.has:<strong>bold</strong>","true"),("ext:src.has:<li>one</li>","true"),
      ("ext:src.has:Some **bold** text","true"),("ext:src.has:Castellum tester","true")])]+Q, 240000)

# S4 long unicode subject + long body
subj="xb llarg — àéíòú ç ñ ü 日本語 🙂 una línia d'assumpte molt llarga que continua i continua fins més enllà de setanta-vuit caràcters {run}"
para="Paràgraf {n}: text llarg amb accents àéíòú, un emoji 🙂 i una URL https://example.org/path/{n}?q=1&r=2 que ha de sobreviure. "*4
long="\n\n".join(para.replace("{n}",str(i)) for i in range(60))+"\n\n"+("x"*1500)+"\n\nEND-OF-LONG-BODY"
sc("xb-long-unicode-send", ["compose","a long non-ASCII subject and a 30 kB body with a 1500-character line arrive intact"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","bob@local.test"), fld("subject",subj), body(long), pause(500), W, cap("long-typed"),
  send]+sent+add("bob")+[mbox("bob"), msg("xb llarg"), W, waitx("detail.subject",subj), pause(1500), cap("bob-reads-long",["main"]), src,
  ex([("ext:src.count:END-OF-LONG-BODY","1")])]+Q, 240000)

# S6 switch account mid-compose after the draft was autosaved
s="xb switch {run}"
sc("xb-switch-account-mid-compose", ["compose","switching From to the second account after typing: sent from the second account, no draft left in the first"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+add("bob",True)+[mbox("alice")]+compose()+[
  fld("to","carol@local.test"), fld("subject",s), body("Typed as Alice, sent as Bob."), pause(4000), W, cap("typed-as-alice"),
  waitx("drafts:alice@local.test",s),
  {"op":"ext","id":"setPopup","window":"@compose","find":{"role":"popup","label":"alice@local.test"},"text":"bob@local.test"}, pause(4000), W, cap("switched-to-bob"),
  send]+sent+[pause(2000), W, cap("after-send",["main"]),
  ex([("ext:drafts:alice@local.test",""),("ext:drafts:bob@local.test","")])]
 +recv("carol",s)+[ex([("ext:src.has:From: bob@local.test","true"),("ext:src.has:alice@local.test","false")])]+Q, 300000)

# S7 evaluation mode refusal, then enable writes and send from the same window
s="xb eval then on {run}"
sc("xb-eval-then-enable-send", ["write safety","Send is refused in evaluation mode; after enabling writes the same window sends and the refusal text is gone"],
 start+[pref("undoSendDelay","0")]+add("alice")+compose()+[fld("to","bob@local.test"), fld("subject",s), body("Only after writes are on."),
  send, pause(1500), cap("refused"), ex([("ext:composeWindows","1"),("ext:pendingCount","0")]),
  {"op":"ext","id":"enableWrites","email":"alice@local.test"}, pause(500), send]+sent+[cap("after-send",["main"]), waitx("drafts:alice@local.test","")]
 +recv("bob",s)+[waitx("messageCount:"+s,"1")]+Q, 240000)

# S8 bcc is not leaked; quoted display name with a comma survives
s="xb bcc {run}"
sc("xb-bcc-not-leaked", ["compose","Bcc recipient gets the message but is not named in the copy To/Cc recipients receive; a quoted name with a comma stays one address"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[fld("to","\"Soler, Dave\" <dave@local.test>"), fld("cc","Bob Ferrer <bob@local.test>"), fld("bcc","carol@local.test"),
  fld("subject",s), body("Carol is a hidden recipient."), cap("typed"), send]+sent
 +recv("dave",s)+[cap("dave-reads",["main"]), ex([("ext:src.has:carol@local.test","false"),("ext:src.has:\"Soler, Dave\" <dave@local.test>","true"),("ext:src.has:Bob Ferrer <bob@local.test>","true")])]
 +recv("carol",s)+[cap("carol-reads",["main"]), ex([("ext:src.has:Carol is a hidden recipient.","true")])]+Q, 300000)
