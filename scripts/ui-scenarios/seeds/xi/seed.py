#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xi explorer seeder: delivers hand-written RFC 822 messages to a xi-prefixed GreenMail
mailbox (auto-created on SMTP delivery; login == password == address).
Usage: seed.py <rcpt-localpart> [only-tag ...]   e.g. seed.py xi-r1
Only talks to GreenMail on 127.0.0.1:3025."""
import smtplib, sys, base64, time
from email.utils import formatdate

rcpt = sys.argv[1] + "@local.test"
only = set(sys.argv[2:])
NOW = time.time()
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAK0lEQVR4nO3NQQEAAATAQGTQP5kwSvC7BdjldMdn9XoHAAAAAAAAAAAAhy3gIwFE6inHLwAAAABJRU5ErkJggg==")
msgs = []
def m(tag, raw, frm="xi-sender@local.test"):
    msgs.append((tag, frm, raw.replace("\r\n", "\n").replace("\n", "\r\n").encode("utf-8", "surrogateescape") if isinstance(raw, str) else raw))
def hdr(subject=None, frm='Xi Sender <xi-sender@local.test>', mid=None, age_h=1, extra="", date=True, ctype='text/plain; charset=utf-8'):
    h = ""
    if frm is not None: h += f"From: {frm}\n"
    h += f"To: {rcpt}\n"
    if subject is not None: h += f"Subject: {subject}\n"
    if date: h += f"Date: {formatdate(NOW - age_h*3600)}\n"
    h += f"Message-ID: <{mid or ('xi-%d-%d' % (len(msgs), int(NOW)))}@xi.local.test>\n"
    h += "MIME-Version: 1.0\n"
    if ctype: h += f"Content-Type: {ctype}\n"
    return h + extra

m("plain", hdr("xi plain hello", age_h=1) + "\nXI-PLAIN-BODY first line.\nSee https://example.org/docs?a=1&b=2 for details.\n\n-- \nXi Sender\n")
m("remote", hdr("xi html remote image", age_h=2, ctype="text/html; charset=utf-8") +
  "\n<html><body><p>XI-REMOTE-BODY before image</p><img src=\"http://127.0.0.1:9/xi-tracker.gif\" width=\"120\" height=\"40\" alt=\"XI-ALT-TEXT\"><p>after image</p></body></html>\n")
m("cid", hdr("xi inline cid image", age_h=3, ctype='multipart/related; boundary="xiB1"') +
  "\n--xiB1\nContent-Type: text/html; charset=utf-8\n\n<html><body><p>XI-CID-BODY above</p><img src=\"cid:xiimg1@xi\" width=\"64\" height=\"64\"><p>XI-CID-BODY below</p></body></html>\n"
  "--xiB1\nContent-Type: image/png\nContent-Transfer-Encoding: base64\nContent-ID: <xiimg1@xi>\nContent-Disposition: inline; filename=\"red.png\"\n\n" + base64.b64encode(PNG).decode() + "\n--xiB1--\n")
ics = ("BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//xi//EN\nMETHOD:REQUEST\nBEGIN:VEVENT\nUID:xi-evt-1@xi\nDTSTAMP:20261001T090000Z\n"
       "DTSTART:20261012T090000Z\nDTEND:20261012T100000Z\nSUMMARY:XI planning sync\nLOCATION:Sala Gran\\, Manresa\n"
       "ORGANIZER;CN=Xi Sender:mailto:xi-sender@local.test\nATTENDEE;CN=Xi Reader;RSVP=TRUE:mailto:%s\nEND:VEVENT\nEND:VCALENDAR\n" % rcpt)
m("invite", hdr("xi invite planning sync", age_h=4, ctype='multipart/alternative; boundary="xiB2"') +
  "\n--xiB2\nContent-Type: text/plain; charset=utf-8\n\nXI-INVITE-BODY you are invited.\n--xiB2\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\n\n" + ics + "--xiB2--\n")
prev = None; refs = []
for i in range(12):
    mid = f"xi-thread-{i}"
    extra = ""
    if prev:
        extra = f"In-Reply-To: <{prev}@xi.local.test>\nReferences: " + " ".join(f"<{r}@xi.local.test>" for r in refs) + "\n"
    who = ["Anna Thread <xi-anna@local.test>", "Bernat Thread <xi-bernat@local.test>", "Clara Thread <xi-clara@local.test>"][i % 3]
    m("thread", hdr(("Re: " if i else "") + "xi long thread topic", frm=who, mid=mid, age_h=40 - i, extra=extra) + f"\nXI-THREAD-BODY-{i:02d} reply number {i}.\n\n> quoted line from before\n")
    prev = mid; refs.append(mid)
m("enc", hdr("=?UTF-8?B?" + base64.b64encode("xi enc Reunió d'equip — demà 🚀".encode()).decode() + "?=", frm="=?ISO-8859-1?Q?Jos=E9_Mar=EDa_N=FA=F1ez?= <xi-jose@local.test>", age_h=5) + "\nXI-ENC-BODY\n")
m("enc", hdr("=?UTF-8?Q?xi_enc_split_Informaci=C3=B3?=\n =?UTF-8?Q?_del_pressup?=\n =?ISO-8859-1?Q?ost_d'estiu_=E0_la_carta?=", frm='"Riera, Joan Marc" <xi-comma@local.test>', age_h=6) + "\nXI-ENC2-BODY\n")
m("lang", hdr("xi catala latin1 qp", age_h=7, ctype="text/plain; charset=iso-8859-1", extra="Content-Transfer-Encoding: quoted-printable\n") +
  "\nXI-CAT-BODY Bon dia, l'=E0via Concepci=F3 far=E0 canelons dem=E0. Qu=E8 et sembla=\n si hi anem? Pa=EDs, gar=E7on, =B7 l=B7l geminada: col=B7legi.\n")
m("lang", (hdr("xi espanol cp1252 8bit", age_h=8, ctype="text/plain; charset=windows-1252", extra="Content-Transfer-Encoding: 8bit\n")).encode() +
  "\nXI-ES-BODY ¿Qué tal, señor Núñez? Son 25 € — “comillas” y ¡ánimo!\n".encode("cp1252"))
LONGSUBJ = "xi very long subject " + ("Lorem ipsum dolor sit amet consectetur " * 12) + "ENDMARK"
LONGNAME = "Xi " + ("Maria-Concepcio de la Santissima Trinitat " * 5) + "Lastname"
m("long", hdr(LONGSUBJ, frm=f'"{LONGNAME}" <xi-a-very-long-local-part-that-goes-on-and-on-and-on-and-on-and-on@a-very-long-subdomain.of-another-long-domain.local.test>', age_h=9) + "\nXI-LONG-BODY\n")
m("long", hdr("xi nospace " + "A" * 600 + "ENDMARK", age_h=9.5) + "\nXI-LONG2-BODY " + "W" * 3000 + " END\n")
m("nosubj", hdr(None, age_h=10) + "\nXI-NOSUBJECT-BODY this message has no Subject header.\n")
m("nosubj", hdr("", age_h=10.2) + "\nXI-EMPTYSUBJECT-BODY this message has an empty Subject header.\n")
m("nofrom", hdr("xi no from header", frm=None, age_h=11) + "\nXI-NOFROM-BODY this message has no From header.\n")
m("auth", hdr("xi auth all pass", age_h=12, extra="Authentication-Results: mx.local.test; spf=pass smtp.mailfrom=local.test; dkim=pass header.d=local.test; dmarc=pass header.from=local.test\n") + "\nXI-AUTHPASS-BODY\n")
m("auth", hdr("xi auth all fail", frm='"PayPal Security" <xi-phish@local.test>', age_h=13, extra="Authentication-Results: mx.local.test; spf=fail smtp.mailfrom=evil.test; dkim=fail header.d=evil.test; dmarc=fail header.from=local.test\n") + "\nXI-AUTHFAIL-BODY\n")
m("auth", hdr("xi spoofed display name", frm='"service@paypal.com" <xi-attacker@local.test>', age_h=13.5) + "\nXI-SPOOF-BODY\n")
m("date", hdr("xi no date header", date=False) + "\nXI-NODATE-BODY\n")
m("date", hdr("xi future date", age_h=-24*365*5) + "\nXI-FUTURE-BODY\n")
m("empty", hdr("xi empty body", age_h=14) + "\n")
m("hostile", hdr("xi html hostile", age_h=15, ctype="text/html; charset=utf-8") +
  "\n<html><head><meta http-equiv=\"refresh\" content=\"0;url=http://127.0.0.1:9/xi-refresh\"><style>body{background:url(http://127.0.0.1:9/xi-bg.png)}</style></head>"
  "<body onload=\"document.title='XI-PWNED'\"><p>XI-HOSTILE-BODY visible</p><script>document.write('XI-SCRIPT-RAN')</script>"
  "<a href=\"http://127.0.0.1:9/evil\">https://bank.example.com/login</a><form action=\"http://127.0.0.1:9/f\"><input name=p type=password><input type=submit value=XI-SUBMIT></form>"
  "<iframe src=\"http://127.0.0.1:9/frame\"></iframe></body></html>\n")
m("dupid", hdr("xi dupid first", mid="xi-dup-same", age_h=16) + "\nXI-DUP-BODY-ONE\n")
m("dupid", hdr("xi dupid second", mid="xi-dup-same", age_h=16.1) + "\nXI-DUP-BODY-TWO\n")
m("nocharset", (hdr("xi no charset 8bit", age_h=17, ctype="text/plain")).encode() + "\nXI-NOCS-BODY café señor\n".encode("utf-8"))
m("nocharset", (hdr("xi latin1 body mislabelled none", age_h=17.2, ctype=None)).encode() + "\nXI-NOCT-BODY caf\xe9 se\xf1or\n".encode("latin-1"))
m("rfc822", hdr("xi forwarded as attachment", age_h=18, ctype='multipart/mixed; boundary="xiB3"') +
  "\n--xiB3\nContent-Type: text/plain; charset=utf-8\n\nXI-FWD-OUTER see attached.\n--xiB3\nContent-Type: message/rfc822\nContent-Disposition: attachment; filename=\"orig.eml\"\n\n"
  "From: inner@local.test\nTo: x@local.test\nSubject: xi INNER subject\nContent-Type: text/plain\n\nXI-FWD-INNER body\n--xiB3--\n")
m("alt", hdr("xi alternative text and html differ", age_h=19, ctype='multipart/alternative; boundary="xiB4"') +
  "\n--xiB4\nContent-Type: text/plain; charset=utf-8\n\nXI-ALT-TEXTPART\n--xiB4\nContent-Type: text/html; charset=utf-8\n\n<html><body><h1>XI-ALT-HTMLPART</h1></body></html>\n--xiB4--\n")
m("b64", hdr("xi base64 utf8 body", age_h=20, extra="Content-Transfer-Encoding: base64\n") + "\n" + base64.encodebytes("XI-B64-BODY El pingüí menja peix. 日本語 ✓\n".encode()).decode())
m("rtl", hdr("=?UTF-8?B?" + base64.b64encode("xi rtl ‮gnp.exe invoice".encode()).decode() + "?=", age_h=21) + "\nXI-RTL-BODY\n")
m("multifrom", hdr("xi two from addresses", frm="One <xi-one@local.test>, Two <xi-two@local.test>", age_h=22) + "\nXI-MULTIFROM-BODY\n")

AR = "Authentication-Results: %s; spf=%s smtp.mailfrom=paypal.com; dkim=%s header.d=paypal.com; dmarc=%s header.from=paypal.com\n"
m("trust", hdr("xi trust forged suffix id", frm='"PayPal" <service@paypal.com>', age_h=1, extra=AR % ("1","pass","pass","pass")) + "\nXI-TRUST-FORGED body: attacker-authored header whose authserv-id is only a suffix label of the IMAP host.\n")
m("trust", hdr("xi trust exact host fail", age_h=2, extra=AR % ("127.0.0.1","fail","fail","fail")) + "\nXI-TRUST-FAIL\n")
m("trust", hdr("xi trust exact host pass", age_h=3, extra=AR % ("127.0.0.1","pass","pass","pass")) + "\nXI-TRUST-PASS\n")
m("trust", hdr("xi trust none", age_h=4) + "\nXI-TRUST-NONE\n")
m("trust", hdr("xi trust received-spf forged", age_h=5, extra="Received-SPF: pass (attacker says 127.0.0.1 is fine) client-ip=6.6.6.6;\n") + "\nXI-TRUST-RSPF\n")

def inv(name, extra_ev="", dtstart="DTSTART:20261012T090000Z\nDTEND:20261012T100000Z\n", pre=""):
    ics = ("BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//xi//EN\nMETHOD:REQUEST\n" + pre + "BEGIN:VEVENT\nUID:xi-evt-%s@xi\nDTSTAMP:20261001T090000Z\n" % name
           + dtstart + "SUMMARY:XI inv %s\n" % name + extra_ev +
           "ORGANIZER;CN=Xi Sender:mailto:xi-sender@local.test\nATTENDEE;CN=Xi Reader;RSVP=TRUE:mailto:%s\nEND:VEVENT\nEND:VCALENDAR\n" % rcpt)
    m("inv", hdr("xi inv " + name, age_h=4, ctype='multipart/alternative; boundary="xiB2"') +
      "\n--xiB2\nContent-Type: text/plain; charset=utf-8\n\nXI-INVITE-BODY you are invited.\n--xiB2\nContent-Type: text/calendar; charset=utf-8; method=REQUEST\n\n" + ics + "--xiB2--\n")
inv("bare")
inv("location plain", "LOCATION:Room 1\n")
inv("location comma", "LOCATION:Sala Gran\\, Manresa\n")
inv("description escapes", "DESCRIPTION:Line one\\nLine two\\; with semicolon\\, comma\n")
inv("tzid", dtstart="DTSTART;TZID=Europe/Madrid:20261012T110000\nDTEND;TZID=Europe/Madrid:20261012T120000\n",
    pre="BEGIN:VTIMEZONE\nTZID:Europe/Madrid\nBEGIN:STANDARD\nDTSTART:19701025T030000\nTZOFFSETFROM:+0200\nTZOFFSETTO:+0100\nTZNAME:CET\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\nEND:STANDARD\nEND:VTIMEZONE\n")
inv("allday", dtstart="DTSTART;VALUE=DATE:20261012\nDTEND;VALUE=DATE:20261013\n")
inv("folded", "DESCRIPTION:This is a long description that is folded across\n  two lines per RFC 5545\n")

def rem(name, html, frm='Xi Sender <xi-sender@local.test>'):
    m("rem", hdr("xi rem " + name, frm=frm, age_h=2, ctype="text/html; charset=utf-8") + "\n<html><body><p>XI-REM-" + name.upper().replace(" ","-") + "</p>" + html + "</body></html>\n")
U = "http://127.0.0.1:9/xi-"
rem("img", '<img src="%simg.gif" width=50 height=20>' % U)
rem("img other sender", '<img src="%simg2.gif" width=50 height=20>' % U, frm='Other <xi-other@local.test>')
rem("img same sender again", '<img src="%simg3.gif" width=50 height=20>' % U)
rem("td background", '<table><tr><td background="%sbg.gif" width=80 height=30>cell</td></tr></table>' % U)
rem("style url", '<div style="background-image:url(%sstyle.gif);width:80px;height:30px">styled</div>' % U)
rem("srcset only", '<img srcset="%sa.gif 1x, %sb.gif 2x" width=50 height=20 alt="srcset">' % (U,U))
rem("protocol relative", '<img src="//127.0.0.1:9/xi-rel.gif" width=50 height=20>')
rem("uppercase scheme", '<img src="HTTP://127.0.0.1:9/xi-up.gif" width=50 height=20>')
rem("data uri only", '<img src="data:image/png;base64,%s" width=32 height=32>' % base64.b64encode(PNG).decode())
rem("link only no images", '<a href="%spage">a plain link</a>' % U)
rem("video poster", '<video poster="%sposter.gif" width=80 height=40></video><audio src="%sa.mp3"></audio>' % (U,U))
rem("svg image", '<svg width="40" height="40"><image href="%ssvg.gif" width="40" height="40"/></svg>' % U)

s = smtplib.SMTP("127.0.0.1", 3025)
n = 0
for tag, frm, raw in msgs:
    if only and tag not in only: continue
    s.sendmail(frm, [rcpt], raw); n += 1
s.quit()
print(f"delivered {n} messages to {rcpt}")
