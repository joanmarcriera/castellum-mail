#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Seed xf-s@local.test with six mails of known senders and subjects for the list-sort scenario (#3043).
Usage: python3 scripts/ui-scenarios/seeds/xf/seed_sort.py   (GreenMail on 127.0.0.1 only; touches only xf-s@local.test).
Accents, case and Re:/Fwd: prefixes are deliberate: the sort must fold them (Alex < Ana < bob < Oscar < Zoe)."""
import imaplib, smtplib, time, unicodedata
from email.message import EmailMessage
from email.utils import formatdate, formataddr
A = "xf-s@local.test"
MAILS = [  # (hours ago, sender name, subject)
    (6, "Zoe Zed", "Re: Zebra crossing"), (5, "Àlex Roca", "apple pie"), (4, "bob Brown", "Banana bread"),
    (3, "Óscar Gil", "Émile letter"), (2, "Ana Puig", "Fwd: carrots"), (1, "Ana Puig", "Dates")]
def send(subj, name, age):
    m = EmailMessage(); ascii_local = unicodedata.normalize("NFKD", name.split()[0]).encode("ascii", "ignore").decode().lower()
    m["From"] = formataddr((name, f"{ascii_local}@sort.test"), charset="utf-8"); m["To"] = A
    m["Subject"] = subj; m["Date"] = formatdate(time.time() - age * 3600, localtime=True)
    m["Message-ID"] = f"<xf-sort-{time.time_ns()}@local.test>"; m.set_content(f"Body of {subj}\n")
    with smtplib.SMTP("127.0.0.1", 3025) as s: s.send_message(m)
send("XF sort bootstrap", "Boot Strap", 0); time.sleep(1)
im = imaplib.IMAP4("127.0.0.1", 3143); im.login(A, A)
for b in im.list()[1]:
    name = b.decode().split(' "." ')[-1].strip('"')
    if im.select(f'"{name}"')[0] != "OK": continue
    im.store("1:*", "+FLAGS", "\\Deleted"); im.expunge(); im.close()
im.logout()
for age, name, subj in MAILS: send(subj, name, age)
time.sleep(1)
im = imaplib.IMAP4("127.0.0.1", 3143); im.login(A, A); print(A, "INBOX", im.select("INBOX", readonly=True)[1][0].decode()); im.logout()
