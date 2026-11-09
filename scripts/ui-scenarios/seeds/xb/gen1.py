# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from h import *
sc("xb-defect-erased-compose-leaves-draft", ["compose","a new message typed then erased and closed must not leave a draft behind"],
 start+add("alice")+compose()+[fld("subject","xb changed my mind {run}"), body("Half a thought."), pause(300), cap("typed"),
  body(""), fld("subject",""), pause(200), cap("erased"), close, waitx("composeWindows","0"), pause(1500), W, cap("after-close",["main"]),
  ex([("ext:drafts:alice@local.test","")])]+Q)

sc("xb-eval-send-refused", ["compose","evaluation mode refuses Send and Send Later with the enable-writes hint; nothing queued; draft kept"],
 start+add("alice")+compose()+[fld("to","bob@local.test"), fld("subject","xb eval refused {run}"), body("This must never leave the machine."),
  pause(3500), W, cap("typed-eval-mode"), ex([("ext:pendingCount","0")]),
  send, pause(1500), cap("after-send-refused"),
  ex([("ext:composeWindows","1"),("ext:pendingCount","0"),("ext:undoBanner","hidden"),("ext:body","This must never leave the machine.")]),
  {"op":"assert","window":"compose","expect":[{"find":{"text":"evaluation mode"},"exists":True},{"find":{"text":"Enable writes"},"exists":True}]},
  {"op":"command","id":"sendLaterInOneHour","allowServerWrites":True}, pause(1500), cap("after-send-later-refused"),
  ex([("ext:composeWindows","1"),("ext:pendingCount","0")]),
  close, waitx("composeWindows","0"), pause(500),
  {"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:outbox"}, W, cap("outbox-empty"),
  waitx("drafts:alice@local.test","xb eval refused {run}"), ex([("ext:pendingCount","0")])]+Q)

# D1: invalid recipients are named, nothing is sent, window stays
sc("xb-invalid-recipients", ["compose","invalid / missing recipients are reported inline and nothing is queued"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[
  fld("to","bob@local.test, not-an-address"), fld("subject","xb invalid {run}"), body("Body."), send, pause(800), cap("invalid-to"),
  {"op":"assert","window":"compose","expect":[{"find":{"text":"Not a valid address: not-an-address"},"exists":True}]},
  ex([("ext:composeWindows","1"),("ext:pendingCount","0")]),
  fld("to",""), fld("cc","bob@local.test"), send, pause(800), cap("cc-only"),
  {"op":"assert","window":"compose","expect":[{"find":{"text":"Add at least one To recipient."},"exists":True}]},
  ex([("ext:composeWindows","1"),("ext:pendingCount","0")]),
  fld("to","bob@local.test; Carol Vidal <carol@local.test>;"), fld("cc",""), send,
  waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
 +add("carol")+[mbox("carol"), msg("=xb invalid {run}"), W, cap("carol-got-it",["main"]), src,
  ex([("ext:src.has:bob@local.test","true"),("ext:src.has:Carol Vidal","true")])]+Q, 240000)

# D2: duplicates in the recipient list -> one delivery, one header entry
sc("xb-duplicate-recipients", ["compose","the same address typed twice (To and Cc, different case) is delivered once"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+compose()+[
  fld("to","bob@local.test, bob@local.test, Bob Ferrer <BOB@local.test>"), fld("cc","bob@local.test"),
  fld("subject","xb dup {run}"), body("Once is enough."), cap("dup-typed"), send,
  waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
 +add("bob")+[mbox("bob"), msg("=xb dup {run}"), W, pause(3000), cap("bob-inbox",["main"]), src,
  ex([("ext:messageCount:xb dup {run}","1")]),
  ex([("ext:src.count:bob@local.test","1")])]+Q, 240000)

# E: send from the second account
sc("xb-second-account-send", ["compose","From picker: sending from the second account uses that account, its signature and its write policy"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+add("bob",True)+[
  pref("signature:alice@local.test","-- \nAlice sig"), pref("signature:bob@local.test","-- \nBob sig"),
  mbox("alice")]+compose()+[cap("new-from-alice"), ex([("ext:fromPopup","alice@local.test"),("ext:bodyHas:Alice sig","true")]),
  {"op":"ext","id":"setPopup","window":"@compose","find":{"role":"popup","label":"alice@local.test"},"text":"bob@local.test"}, W,
  ex([("ext:fromPopup","bob@local.test"),("ext:bodyHas:Bob sig","true"),("ext:bodyHas:Alice sig","false")]),
  fld("to","carol@local.test"), fld("subject","xb from bob {run}"), {"op":"ext","id":"setBody","window":"@compose","text":"Sent from the second account.\n\n-- \nBob sig"}, cap("from-bob"), send,
  waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
 +add("carol")+[mbox("carol"), msg("=xb from bob {run}"), W, cap("carol-inbox",["main"]), src,
  ex([("ext:src.has:From: bob@local.test","true"),("ext:src.has:alice@local.test","false")]),
  mbox("bob","sent"), {"op":"ext","id":"waitExt","name":"messageCount:xb from bob {run}","text":"1","timeoutMs":40000}, cap("bob-sent",["main"])]+Q, 300000)
