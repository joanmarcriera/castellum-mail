#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xh explorer seeder: deliver a few messages by SMTP to xh-* GreenMail addresses (auto-creates accounts;
password == email address). Usage: seed.py <addr> <count> [subject-prefix]"""
import smtplib, sys, imaplib
from email.message import EmailMessage
addr, n = sys.argv[1], int(sys.argv[2]); pre = sys.argv[3] if len(sys.argv) > 3 else "XH note"
s = smtplib.SMTP("127.0.0.1", 3025)
for i in range(n):
    m = EmailMessage(); m["From"] = "xh-sender@local.test"; m["To"] = addr; m["Subject"] = f"{pre} {i+1}"
    m.set_content(f"Body of {pre} {i+1}.\n" + ("Lorem ipsum dolor sit amet. " * 40))
    s.send_message(m)
s.quit()
i = imaplib.IMAP4("127.0.0.1", 3143); print(i.login(addr, addr)); print(i.select("INBOX")); i.logout()
