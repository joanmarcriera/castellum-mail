#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer seeder: delivers the xj journey's own mail to GreenMail (127.0.0.1:3025) and sets
server keywords on one message over IMAP (127.0.0.1:3143). Usage: seed.py <account-localpart>
The account <localpart>@local.test is auto-created by GreenMail on first SMTP delivery
(login = the address, password = the address). Only touches that xj-prefixed account."""
import smtplib, imaplib, sys, time
from email.message import EmailMessage
from email.utils import formatdate, make_msgid
acct = sys.argv[1] + "@local.test"
assert acct.startswith("xj-")
def msg(subject, body, frm="News Desk <news@lists.example.test>", headers=None, mid=None, refs=None):
    m = EmailMessage()
    m["From"] = frm; m["To"] = acct; m["Subject"] = subject
    m["Date"] = formatdate(localtime=False); m["Message-ID"] = mid or make_msgid(domain="local.test")
    if refs: m["In-Reply-To"] = refs[-1]; m["References"] = " ".join(refs)
    for k, v in (headers or {}).items(): m[k] = v
    m.set_content(body)
    return m
t1 = make_msgid(domain="local.test"); t2 = make_msgid(domain="local.test"); t3 = make_msgid(domain="local.test")
msgs = [
 msg("XJ Weekly digest (one-click)", "Digest body.\n", headers={
   "List-Unsubscribe": "<https://127.0.0.1:9/unsub?u=xj>, <mailto:xj-unsub@local.test?subject=unsubscribe-xj>",
   "List-Unsubscribe-Post": "List-Unsubscribe=One-Click"}),
 msg("XJ Shop offers (https only)", "Offers body.\n", headers={"List-Unsubscribe": "<https://127.0.0.1:9/classic-unsub>"}),
 msg("XJ Club news (mailto only)", "Club body.\n", headers={"List-Unsubscribe": "<mailto:xj-unsub@local.test?subject=unsubscribe%20me>"}),
 msg("XJ Legacy list (http only)", "Legacy body.\n", headers={"List-Unsubscribe": "<http://127.0.0.1:9/u>"}),
 msg("XJ Roadmap thread", "First.\n", frm="Pau Roadmap <pau@partner.example.test>", mid=t1),
 msg("Re: XJ Roadmap thread", "Second.\n", frm="Pau Roadmap <pau@partner.example.test>", mid=t2, refs=[t1]),
 msg("Re: XJ Roadmap thread", "Third.\n", frm="Nuria Partner <nuria@partner.example.test>", mid=t3, refs=[t1, t2]),
 msg("XJ Long " + ("subject that keeps going and going " * 9).strip(), "Long subject body.\n"),
 msg("XJ Server keywords", "Carries server keywords.\n", frm="Pau Roadmap <pau@partner.example.test>"),
 msg("XJ Plain note", "A plain note to snooze.\n", frm="Pau Roadmap <pau@partner.example.test>"),
]
with smtplib.SMTP("127.0.0.1", 3025, timeout=20) as s:
    for m in msgs:
        s.send_message(m, from_addr="xj-seeder@local.test", to_addrs=[acct]); time.sleep(0.05)
im = imaplib.IMAP4("127.0.0.1", 3143); im.login(acct, acct)
typ, d = im.select("INBOX"); print("INBOX", typ, d)
print("permanentflags", im.response("PERMANENTFLAGS"))
typ, d = im.search(None, 'SUBJECT', '"XJ Server keywords"'); n = d[0].split()[-1]
print("store", im.store(n, "+FLAGS", "($Forwarded NonJunk xjServerTag)"))
print("flags", im.fetch(n, "(FLAGS)"))
im.logout()
