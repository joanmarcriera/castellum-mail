#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xc explorer seeder: delivers reply/forward fixtures by SMTP to GreenMail (127.0.0.1:3025).
Usage: python3 build/explore/xc/seed.py   (idempotent per subject: skips subjects already in xc-me's INBOX)
Recipient account xc-me@local.test is auto-created by delivery; GreenMail's auto password = the address."""
import smtplib, imaplib, sys
from email.message import EmailMessage
from email.utils import formatdate, make_msgid
ME = "xc-me@local.test"
def existing():
    try:
        m = imaplib.IMAP4("127.0.0.1", 3143); m.login(ME, ME); m.select("INBOX", readonly=True)
        _, d = m.fetch("1:*", "(BODY.PEEK[HEADER.FIELDS (SUBJECT)])")
        m.logout()
        return b" ".join(x[1] for x in d if isinstance(x, tuple)).decode("utf-8", "replace")
    except Exception as e:
        return ""
have = existing()
def send(subject, frm="Anna Puig <xc-anna@local.test>", to=ME, cc=None, reply_to=None, text=None, html=None, att=None, rcpt=None, extra=None):
    if subject in have: print("skip", subject); return
    m = EmailMessage()
    m["From"] = frm; m["To"] = to
    if cc: m["Cc"] = cc
    if reply_to: m["Reply-To"] = reply_to
    m["Subject"] = subject; m["Date"] = formatdate(localtime=True); m["Message-ID"] = make_msgid(domain="xc.local.test")
    for k, v in (extra or {}).items(): m[k] = v
    if text is not None:
        m.set_content(text)
        if html: m.add_alternative(html, subtype="html")
    elif html: m.set_content(html, subtype="html")
    for name, ctype, data in att or []:
        a, b = ctype.split("/"); m.add_attachment(data, maintype=a, subtype=b, filename=name)
    with smtplib.SMTP("127.0.0.1", 3025) as s: s.send_message(m, to_addrs=rcpt or [ME])
    print("sent", subject)
PNG = bytes.fromhex("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360f8cfc0000003010100c9fe92ef0000000049454e44ae426082")
send("XC plain note", text="Hi,\n\nCan you confirm the meeting on Friday?\n\nAnna\n", cc="Carl Roca <xc-carl@local.test>, XC-Me@Local.Test")
send("XC replyto list", text="Please reply to the list, not to me.\n", reply_to="XC List <xc-list@local.test>")
send("RE: XC upper prefix", text="Upper-case RE prefix.\n")
send("Re: Re: XC doubled prefix", text="Doubled prefix.\n")
send("FW: XC outlook forward", text="Outlook style forward prefix.\n")
send("Fwd: XC already forwarded", text="Already forwarded once.\n")
send("RV: XC reenviado", text="Prefijo de reenvio en castellano.\n")
send("XC html only", html="<html><body><h1>Quarterly numbers</h1><p>Revenue is <b>up 12%</b> on last quarter.</p><p>Regards,<br>Anna</p></body></html>")
send("XC pressupost amb adjunt", text="Bon dia,\n\nT'adjunto el pressupost revisat. Ja em diràs què et sembla.\n\nSalutacions,\nAnna\n",
     att=[("pressupost-2026.pdf", "application/pdf", b"%PDF-1.4\n% xc fixture\n" + b"0" * 2048), ("foto botiga.png", "image/png", PNG)])
send("XC factura con adjunto", frm="Luis Gómez <xc-luis@local.test>", text="Hola,\n\nAdjunto la factura de marzo.\n\nUn saludo,\nLuis\n",
     att=[("factura-marzo.pdf", "application/pdf", b"%PDF-1.4\n% xc fixture\n" + b"1" * 1024)])
send("XC see attached but none", text="Hi,\n\nPlease see the attached file for details.\n\nAnna\n")
send("XC Attachment policy question", text="What is our policy here?\n")
send("XC long quote", text="\n".join("Line %03d of a very long message that keeps going well past the usual column width so wrapping can be seen in the quote block." % i for i in range(1, 401)) + "\n")
send("XC from myself", frm="Me Myself <xc-me@local.test>", to="Bob <bob@local.test>, Carol <carol@local.test>", text="A note I sent to Bob and Carol.\n")
send("", text="This message has no subject at all.\n", extra={"X-XC": "nosubject"})
send("XC quoted name", frm='"Roca, Carl (Sales)" <xc-carl@local.test>', cc="Anna Puig <xc-anna@local.test>", text="From a sender whose display name has a comma.\n")
send("XC inline image", text="Logo below.\n", html='<html><body><p>Logo below.</p><img src="cid:xclogo"></body></html>')
send("XC twelve attachments", text="Many files.\n", att=[("file-%02d-with-a-rather-long-descriptive-name.txt" % i, "text/plain", b"x" * 100) for i in range(1, 13)])
