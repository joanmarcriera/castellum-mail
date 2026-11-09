# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from h import *
ob=[{"op":"openWindow","target":"outbox"},{"op":"wait","until":"window:title:Outbox"},W]
OB="title:Outbox"
first=start+[pref("undoSendDelay","0")]+add("alice",True,smtp="127.0.0.1:3925")+compose()+[
  fld("to","bob@local.test"), fld("subject","xb retry {run}"), body("Fails once, then the server is back."), send,
  waitx("pendingCount","1",30000), pause(1500), W, cap("send-failed"), close, waitx("composeWindows","0")]
note="NEEDS build/explore/xb/flaky_smtp_proxy.py on 127.0.0.1:3925 (run via build/explore/xb/run_with_proxy.sh)"
sc("xb-defect-outbox-retry-does-not-send", ["outbox","Retry on a failed send must attempt delivery again", note],
 first+ob+[cap("outbox-failed"),
  {"op":"accounts","id":"click","window":OB,"text":"477,50"}, pause(1000), cap("after-retry-click"),
  waitx("pendingCount","0",25000)]+Q)
sc("xb-outbox-next-send-flushes-failed", ["outbox","a failed send is delivered by the next successful Send (the only drain there is)", note],
 first+compose()+[fld("to","bob@local.test"), fld("subject","xb retry second {run}"), body("Second message."), send,
  waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
 +add("bob")+[mbox("bob"), msg("=xb retry {run}"), {"op":"ext","id":"waitExt","name":"messageCount:xb retry","text":"2","timeoutMs":60000}, cap("bob-has-both",["main"])]+Q, 300000)
