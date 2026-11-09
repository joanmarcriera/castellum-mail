# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xb pass 2i: repeatable version of the 'next send flushes the failed one' scenario (per-run subjects). Needs the proxy: run_with_proxy.sh. Usage: python3 gen13.py"""
from h import *
note="NEEDS build/explore/xb/flaky_smtp_proxy.py on 127.0.0.1:3925 (run via build/explore/xb/run_with_proxy.sh)"
a,b="xb flush first {run}","xb flush second {run}"
sc("xb-outbox-failed-send-flushed-by-next-send", ["outbox","a send that failed once is delivered, once, by the next successful Send (today the only thing that drains a failed send)", note],
 start+[pref("undoSendDelay","0")]+add("alice",True,smtp="127.0.0.1:3925")+compose()+[
  fld("to","bob@local.test"), fld("subject",a), body("Fails once, then the server is back."), send,
  waitx("pendingCount","1",30000), pause(1500), W, cap("send-failed"), close, waitx("composeWindows","0")]
 +compose()+[fld("to","bob@local.test"), fld("subject",b), body("Second message."), send,
  waitx("composeWindows","0",20000), waitx("pendingCount","0",30000)]
 +add("bob")+[mbox("bob"), msg("="+a), msg("="+b), W, pause(3000), cap("bob-has-both",["main"]),
  ex([("ext:messageCount:"+a,"1"),("ext:messageCount:"+b,"1")])]+Q, 300000)
