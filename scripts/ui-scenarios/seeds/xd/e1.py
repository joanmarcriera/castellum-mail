# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
qs = ["pressupost", "calcul", "càlcul", "CÀLCUL", "reunio", "ano", "año", "nino", "señor", "Núria", "nuria",
      "from:nuria", "from:Núria", "from:\"Núria Solé\"", "\"quick brown fox\"", "\"brown quick\"", "quick brown", "-fox quick",
      "has:attachment", "attachment:pangolin", "before:2021-01-01", "after:2021-01-01 ocelot", "before:garbage",
      "subject:C++", "C++", "50%", "(limited)", "[promo]", "subject:\"Budget\"", "subject:\"\\\"Budget\\\" v2\"",
      "to:other@xd.example", "is:unread", "is:read", "is:flagged", "in:inbox", "in:sent", "tag:work", "zzzznotfound",
      "\"unterminated quote", "from:", "subject:", "AND", "OR quick", "quick OR zebra", "NOT", "*", "\"\"", "'", "superlongsubjectword",
      "xd-s1", "l'apartat", "d'enginyeria", "enginyeria", "matí", "mati", "PRESSUPOST"]
steps = [XD(), R("selectMailbox", accountRef="xd", text="inbox"), W(), P("messageCount", "subjects"), C("inbox")]
for q in qs: steps += [S(q), A(("reading:searchText", q)), P("messageCount", "subjects", "errorText")]
write("xd-e1", ["explore"], steps, d=".")
