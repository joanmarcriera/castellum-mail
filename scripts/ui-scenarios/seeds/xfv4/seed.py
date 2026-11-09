#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Seed for xf-defect-delete-trashed-from-search-v2: deliver ONE 'XFV4 trashme'
message to xfv4-g@local.test (GreenMail auto-creates the account, password = address).
Run once before every run of the scenario: python3 build/explore/xfv4/seed.py"""
import smtplib
from email.message import EmailMessage
m=EmailMessage(); m['From']='xfv4-sender@local.test'; m['To']='xfv4-g@local.test'
m['Subject']='XFV4 trashme'; m.set_content('trashme body')
with smtplib.SMTP('127.0.0.1',3025) as s: s.send_message(m)
print('seeded')
