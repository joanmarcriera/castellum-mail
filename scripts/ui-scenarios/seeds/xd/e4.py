# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
import unicodedata
st = ["messageCount", "searchText", "mailboxRole", "smartSelected", "isSnoozedList"]
qs = ["   ", "—", "!!!", "-", ":", "-quick", "has:attachments", "HAS:ATTACHMENT", "FROM:nuria", "is:UNREAD", "in:Inbox", "in:trash",
      "before:2020-01-15", "before:2020-01-16", "before:15/01/2020", "after:2020-01-15 before:2020-01-16", "date:2020", "date:2020-01", "date:2020-02",
      unicodedata.normalize("NFD", "càlcul"), unicodedata.normalize("NFD", "año"), "Solé", "sole", "from:sole", "subject:reunio", "subject:reunió", "subject:año", "subject:ano",
      "from:fox@xd.example", "from:=fox@xd.example", "from:xd.example", "to:xd-s1", "pangolin-report", "attachment:pdf", "filename:pangolin-report.pdf",
      "quick   brown", " quick ", "QUICK", "size:>1", "size:<10", "within:1d", "within:30d", "is:unread is:read", "-is:unread", "tag:", "is:", "is:vip", "is:muted"]
steps = [XD(), PERS("alice"), R("selectMailbox", accountRef="xd", text="inbox"), W(), R("searchScope", text="mailbox"), W()]
for q in qs: steps += [S(q), P("messageCount", "subjects", "errorText")]
steps += [S(""),
  # race: search (all accounts) then immediately switch mailbox
  R("searchScope", text="all"), {"op": "search", "text": "is:unread"}, R("selectMailbox", accountRef="xd", text="inbox"), W(), P(*st),
  S(""), R("selectMailbox", accountRef="xd", text="inbox"), W(),
  # selected message then a search that excludes it
  R("searchScope", text="mailbox"), R("selectMessage", text="xd phrase one"), W(), S("zebra"), P("selectedSubject", "subjects"), C("search-excludes-open-message"),
  S(""),
  # duplicate smart name
  R("upsertSmart", name="xd dup", text="quick"), W(),
  # min window with search results
  S("quick"), {"op": "resizeWindow", "window": "main", "contentSize": "min"}, W(), C("min-window-search"),
  A(window="main", rules=["nonBlank", "fitsScreen", "controlsReachable"]),
  R("upsertSmart", name="xd dup", text="zebra"), W(),
]
write("xd-e4", ["explore"], steps, d=".")
