#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Seed xd explorer mail into GreenMail (127.0.0.1:3025) for xd-s1@local.test. Run ONCE (re-runs duplicate)."""
import smtplib, sys
from email.message import EmailMessage
from email.utils import format_datetime
from datetime import datetime, timezone, timedelta
TO = "xd-s1@local.test"
now = datetime.now(timezone.utc)
def m(subj, body, frm, age_days=1, to=TO, attach=None, date=None):
    e = EmailMessage()
    e["Subject"] = subj; e["From"] = frm; e["To"] = to
    e["Date"] = format_datetime(date or (now - timedelta(days=age_days)))
    e["Message-ID"] = f"<xd-{abs(hash(subj))}@xd.example>"
    e.set_content(body)
    if attach: e.add_attachment(b"%PDF-1.4 xd fake", maintype="application", subtype="pdf", filename=attach)
    return e
msgs = [
 m("Reunió de pressupost — àrea d'enginyeria", "Hola,\n\nEl càlcul del pressupost és a punt. Parlem-ne demà al matí.\n\nNúria", '"Núria Solé" <nuria@xd.example>', 2),
 m("Presupuesto año próximo", "Estimado señor,\n\nel niño vendrá mañana con el presupuesto.\n\nJosé", '"José Muñoz" <jose@xd.example>', 3),
 m("xd phrase one", "the quick brown fox jumps over the lazy dog", "fox@xd.example", 4),
 m("xd phrase two", "quick is the fox, brown is the dog", "fox@xd.example", 5),
 m('Re: "Budget" v2: final', "budget numbers attached, zebra", "boss@xd.example", 6),
 m("xd report with attachment", "see the attached pangolin report", "reports@xd.example", 7, attach="pangolin-report.pdf"),
 m("C++ and 50% off (limited) [promo]", "special characters in the subject line: narwhal", "promo@xd.example", 8),
 m("xd old message from 2020", "an old ocelot message", "old@xd.example", date=datetime(2020,1,15,12,0,tzinfo=timezone.utc)),
 m("xd long " + "superlongsubjectword " * 30, "long subject quokka", "long@xd.example", 9),
 m("xd cc test", "message to someone else, wombat", "cc@xd.example", 10, to="xd-s1@local.test, other@xd.example"),
]
s = smtplib.SMTP("127.0.0.1", 3025)
for e in msgs: s.send_message(e)
s.quit(); print("sent", len(msgs))
