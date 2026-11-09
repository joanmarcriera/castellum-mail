# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xi passing-coverage scenarios (regenerate with: python3 mk_pass.py)."""
from gen import *
P=lambda ms: {"op":"ext","id":"pause","text":str(ms),"timeoutMs":ms+20000}
BIG={"op":"resizeWindow","window":"main","contentSize":[1270,720]}
def settle(ref="xi"): return [BIG, inbox(ref), P(8000), rd("selectUnified",text="inbox"), idle, inbox(ref)]
def srch(t): return [{"op":"search","text":t}, idle, P(800)]
T=lambda s: "true"; 
def has(*k): return [("reading:detailTextHas:"+x,"true") for x in k]
def hasnt(*k): return [("reading:detailTextHas:"+x,"false") for x in k]
OK=[("reading:errorText","none")]

write("xi-read-charsets-plain", ["reading plain text","Catalan ISO-8859-1 quoted-printable","Spanish windows-1252 8bit","base64 UTF-8","body search finds decoded text"],
 [acct("xi-r4"), settle(),
  sel("xi plain hello"), cap("plain",["main"]), exp([("reading:selectedSubject","xi plain hello")]+has("XI-PLAIN-BODY first line.","https://example.org/docs?a=1&b=2")+OK),
  sel("xi catala latin1 qp"), cap("catala-latin1",["main"]), exp(has("l'àvia Concepció farà canelons demà. Què et sembla si hi anem?","col·legi","garçon")+hasnt("=E0","Ã")),
  sel("xi espanol cp1252 8bit"), cap("espanol-cp1252",["main"]), exp(has("¿Qué tal, señor Núñez? Son 25 € — “comillas” y ¡ánimo!")+hasnt("�")),
  sel("xi base64 utf8 body"), cap("base64-utf8",["main"]), exp(has("El pingüí menja peix. 日本語 ✓")),
  sel("xi no charset 8bit"), exp(has("XI-NOCS-BODY café señor")),
  srch("canelons"), exp([("reading:messageCount","1"),("reading:cell:subject:0","xi catala latin1 qp")]),
  srch("pingüí"), exp([("reading:messageCount","1"),("reading:cell:subject:0","xi base64 utf8 body")]), srch(""), cap("after-search-cleared"), exp([("reading:messageCount","40")])])

LONGSUBJ = "xi very long subject " + ("Lorem ipsum dolor sit amet consectetur " * 12) + "ENDMARK"
write("xi-encoded-and-long-headers", ["RFC 2047 subjects and names (B, Q, split words, mixed charsets)","quoted display name with comma","very long subject and sender","RTL override in subject"],
 [acct("xi-r4"), settle(),
  srch("xi enc Reunió"), exp([("reading:cell:subject:0","xi enc Reunió d'equip — demà 🚀"),("reading:cell:from:0","José María Núñez")]),
  srch("xi enc split"), exp([("reading:cell:subject:0","xi enc split Informació del pressupost d'estiu à la carta"),("reading:cell:from:0","Riera, Joan Marc")]),
  srch(""), exp([("reading:hasSubject:=?","false"),("reading:hasSubject:?=","false")]),
  sel("xi enc Reunió"), cap("encoded-open",["main"]), exp([("reading:selectedSubject","xi enc Reunió d'equip — demà 🚀")]+has("XI-ENC-BODY")),
  sel("xi very long subject"), cap("long-subject-and-sender",["main"]), {"op":"assert","window":"main","rules":["nonBlank","controlsReachable"],"expect":[{"model":"reading:selectedSubject","equals":LONGSUBJ},{"model":"reading:detailTextHas:XI-LONG-BODY","equals":"true"}]},
  sel("xi nospace"), cap("unbroken-600-char-subject",["main"]), {"op":"assert","window":"main","rules":["nonBlank","controlsReachable"],"expect":[{"model":"reading:detailTextHas:XI-LONG2-BODY","equals":"true"}]},
  sel("xi two from addresses"), cap("two-from",["main"]), exp(has("XI-MULTIFROM-BODY")+OK)])

write("xi-missing-headers-and-empty-body", ["no Subject header","empty Subject","no From header","no Date header","future Date","empty body","duplicate Message-ID keeps both messages"],
 [acct("xi-r4"), settle(), exp([("reading:messageCount","40")]),
  srch("XI-NOSUBJECT-BODY"), exp([("reading:messageCount","1"),("reading:cell:subject:0","(no subject)")]), cap("no-subject-row"),
  srch("xi no from header"), exp([("reading:cell:from:0","(unknown)")]), sel("xi no from header"), cap("no-from-open",["main"]), exp(has("XI-NOFROM-BODY")+OK),
  srch(""), sel("xi no date header"), cap("no-date-open",["main"]), exp(has("XI-NODATE-BODY")+OK),
  sel("xi future date"), cap("future-date-open",["main"]), exp(has("XI-FUTURE-BODY")+OK),
  sel("xi empty body"), cap("empty-body-open",["main"]), exp([("reading:selectedSubject","xi empty body")]+OK),
  sel("xi dupid first"), exp(has("XI-DUP-BODY-ONE")), sel("xi dupid second"), cap("dup-message-id-second",["main"]), exp(has("XI-DUP-BODY-TWO")+hasnt("XI-DUP-BODY-ONE"))])

W=lambda k,v: ("reading:webBodyHas:"+k, v)
write("xi-remote-content-and-hostile-html", ["S48 remote content policy","remote image blocked with banner","no banner for data: images or plain links","hostile HTML sanitised (script, form, iframe, meta refresh)","multipart/alternative prefers HTML"],
 [acct("xi-rem1","rem"), acct("xi-r4"), BIG, inbox("rem"), P(5000),
  sel("xi rem img"), cap("remote-img-blocked",["main"]), exp([("reading:hasRemoteContent","true"),W("XI-REM-IMG","true")]),
  sel("xi rem protocol relative"), exp([("reading:hasRemoteContent","true")]),
  sel("xi rem style url"), cap("remote-style-url",["main"]), exp([("reading:hasRemoteContent","true")]),
  sel("xi rem data uri only"), cap("data-uri-image",["main"]), exp([("reading:hasRemoteContent","false"),W("XI-REM-DATA-URI-ONLY","true")]),
  sel("xi rem link only no images"), cap("link-only",["main"]), exp([("reading:hasRemoteContent","false")]),
  sel("xi rem td background"), exp([("reading:hasRemoteContent","false")]),
  inbox("xi"), sel("xi html hostile"), cap("hostile",["main"]), exp([W("XI-HOSTILE-BODY visible","true"),W("XI-SCRIPT-RAN","false"),W("XI-SUBMIT","false"),W("XI-PWNED","false"),("reading:hasRemoteContent","false")]+OK),
  sel("xi alternative text and html differ"), cap("alternative",["main"]), exp([W("XI-ALT-HTMLPART","true")]),
  sel("xi inline cid image"), cap("cid-inline",["main"]), exp([W("XI-CID-BODY above","true"),("reading:attachmentCount","1"),("reading:hasRemoteContent","false")])])

NONE="spf=none dkim=none dmarc=none"
write("xi-trust-surface-verdicts", ["S48 trust surface","fail, pass and none verdicts when the authserv-id equals the IMAP host","unmatched authserv-id reads none","spoofed display name still shows the real address"],
 [acct("xi-tr1","tr"), acct("xi-r4"), BIG, inbox("tr"),
  sel("xi trust exact host fail"), cap("fail-badge",["main"]), exp([("reading:trust","spf=fail dkim=fail dmarc=fail")]),
  sel("xi trust exact host pass"), cap("pass-badge",["main"]), exp([("reading:trust","spf=pass dkim=pass dmarc=pass")]),
  sel("xi trust none"), cap("none-badge",["main"]), exp([("reading:trust",NONE)]),
  inbox("xi"), sel("xi auth all pass"), exp([("reading:trust",NONE)]), sel("xi auth all fail"), cap("foreign-authserv-fail-reads-none",["main"]), exp([("reading:trust",NONE)]),
  sel("xi spoofed display name"), cap("spoofed-display-name",["main"]), exp([("reading:trust",NONE)]+has("XI-SPOOF-BODY"))])

write("xi-invite-variants-after-hydration", ["S55 invite card","multipart/alternative text/calendar","LOCATION with escaped comma","TZID with VTIMEZONE","all-day event","folded lines"],
 [acct("xi-inv1"), BIG, inbox("xi"), P(8000)] + sum([[rd("selectMessage",text="xi inv "+t), P(1500), idle, cap(t.replace(" ","-"),["main"]),
   exp([("reading:invite","REQUEST|XI inv %s|xi-sender@local.test|1" % t),("reading:hasCalendarPart","true")])] for t in ["bare","location comma","description escapes","tzid","allday","folded"]], []))

write("xi-switch-messages-no-stale-body", ["switching quickly between messages shows the selected body, never the previous one","switching mailbox clears the reading pane","thread count column"],
 [acct("xi-tr1","tr"), acct("xi-r4"), BIG, inbox("tr"), P(4000),
  rd("selectMessage",text="xi trust exact host fail"), rd("selectMessage",text="xi trust exact host pass"), rd("selectMessage",text="xi trust none"), rd("selectMessage",text="xi trust exact host pass"), rd("selectMessage",text="xi trust forged suffix id"), rd("selectMessage",text="xi trust none"),
  idle, cap("after-six-switches",["main"]), exp([("reading:selectedSubject","xi trust none")]+has("XI-TRUST-NONE")+hasnt("XI-TRUST-PASS","XI-TRUST-FORGED")),
  inbox("xi"), cap("mailbox-switched-pane-empty"), exp([("reading:selectedSubject","none"),("reading:mailboxRole","inbox")]),
  rd("selectMessage",text="xi html remote image"), rd("selectMessage",text="xi plain hello"), rd("selectMessage",text="xi html hostile"), rd("selectMessage",text="xi plain hello"), idle, cap("html-then-plain",["main"]),
  exp([("reading:selectedSubject","xi plain hello"),("reading:hasRemoteContent","false"),("reading:webBodyHas:XI-HOSTILE-BODY","false"),("reading:webBodyHas:XI-REMOTE-BODY","false")]+has("XI-PLAIN-BODY")),
  srch("xi long thread topic"), exp([("reading:messageCount","12"),("reading:cell:thread:0","▾12")]), srch("xi plain hello"), exp([("reading:cell:thread:0","")])])

write("xi-unified-inbox-source-window", ["S34 unified inbox across two accounts","S18 account column","S49 raw source of a message with no From header","search inside the unified inbox"],
 [acct("xi-tr1","tr"), acct("xi-r4"), BIG, rd("selectUnified",text="inbox"), idle, P(6000), rd("selectUnified",text="inbox"), idle, cap("unified"),
  {"op":"assert","window":"main","rules":["nonBlank"],"expect":[{"model":"reading:isUnified","equals":"true"},{"model":"reading:messageCount","equals":"45"},{"model":"reading:columnVisible:account","equals":"true"}]},
  srch("xi trust none"), exp([("reading:messageCount","1"),("reading:cell:account:0","xi-tr1@local.test")]),
  srch("xi no from header"), exp([("reading:messageCount","1"),("reading:cell:account:0","xi-r4@local.test"),("reading:cell:from:0","(unknown)")]),
  sel("xi no from header"), rd("showSource"), {"op":"wait","until":"window:source","timeoutMs":30000}, idle, rd("waitFor",name="sourceHas:XI-NOFROM-BODY",text="true",timeoutMs=30000), cap("source-no-from",["source"]),
  {"op":"assert","window":"source","rules":["nonBlank","resizable","fitsScreen"],"expect":[{"model":"reading:sourceHas:Subject: xi no from header","equals":"true"},{"model":"reading:sourceHas:From:","equals":"false"}]},
  {"op":"closeWindow","window":"source"}, srch(""), exp([("reading:messageCount","45"),("reading:isUnified","true")])])
