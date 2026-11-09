# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xb pass 2b: account switch mid-compose (defect candidate). Usage: python3 gen6.py"""
from h import *
OB="title:Outbox"
ob=[{"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:"+OB},W]
s="xb switch {run}"
sc("xb-defect-switch-account-strands-draft", ["compose","typed as the first account, From switched to the second, sent: nothing may stay queued and the first account must not keep a draft of a message that was sent"],
 start+[pref("undoSendDelay","0")]+add("alice",True)+add("bob",True)+[mbox("alice")]+compose()+[
  fld("to","carol@local.test"), fld("subject",s), body("Typed as Alice, sent as Bob."), pause(4000), W, cap("typed-as-alice"),
  {"op":"ext","id":"setPopup","window":"@compose","find":{"role":"popup","label":"alice@local.test"},"text":"bob@local.test"}, pause(4000), W, cap("switched-to-bob"),
  send, waitx("composeWindows","0",20000), pause(8000), W]+ob+[cap("outbox-after-send"),
  ex([("ext:pendingCount","0")]),
  {"op":"assert","window":"main","expect":[{"find":{"text":"Local Drafts"},"exists":False}]}]+Q, 300000)
