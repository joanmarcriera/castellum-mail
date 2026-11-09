#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xa explorer seed: deliver a few messages by SMTP to xa-* addresses on local GreenMail
(auto-creates the account; its password is the address). Usage: python3 seed.py"""
import smtplib
from email.message import EmailMessage
def send(to, subj, body, frm="xa-sender@local.test"):
    m = EmailMessage(); m["From"] = frm; m["To"] = to; m["Subject"] = subj; m.set_content(body)
    with smtplib.SMTP("127.0.0.1", 3025) as s: s.send_message(m)
for i in range(3): send("xa-one@local.test", f"XA one message {i+1}", f"Body {i+1}")
send("xa-two@local.test", "XA two only message", "Hello two")
send("xa-caps@local.test", "XA caps message", "Hello caps")
print("seeded")
