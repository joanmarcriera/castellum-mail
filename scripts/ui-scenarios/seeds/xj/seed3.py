#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer seeder 3: two list mails for xj-org3@local.test (auto-created on SMTP delivery, password = address).
One unsubscribes by mailto SUBJECT, one by mailto BODY (RFC 6068 body=, what LISTSERV-style lists need). Usage: seed3.py"""
import smtplib, time
from email.message import EmailMessage
from email.utils import formatdate, make_msgid
acct = "xj-org3@local.test"
def msg(subject, unsub):
    m = EmailMessage(); m["From"] = "News Desk <news@lists.example.test>"; m["To"] = acct; m["Subject"] = subject
    m["Date"] = formatdate(localtime=False); m["Message-ID"] = make_msgid(domain="local.test"); m["List-Unsubscribe"] = unsub
    m.set_content("List body.\n"); return m
with smtplib.SMTP("127.0.0.1", 3025, timeout=20) as s:
    for m in [msg("XJ Club news (mailto subject)", "<mailto:xj-unsub3@local.test?subject=unsubscribe%20me>"),
              msg("XJ Listserv (mailto body)", "<mailto:xj-signoff3@local.test?body=SIGNOFF%20XJ-LIST>")]:
        s.send_message(m, from_addr="xj-seeder@local.test", to_addrs=[acct]); time.sleep(0.05)
print("seeded", acct)
