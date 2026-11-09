# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
st = ["messageCount", "searchText", "mailboxRole", "smartSelected", "isSnoozedList", "isUnified", "searchScope"]
steps = [XD(), PERS("alice"),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("searchScope", text="mailbox"), W(),
  S("quick"), P(*st), C("a-search-quick"),
  # (a) switch mailbox with a search in the field
  R("selectMailbox", accountRef="alice", text="inbox"), W(), P(*st), C("a-switched-to-alice-inbox"),
  S(""), R("selectMailbox", accountRef="xd", text="inbox"), W(),
  # (d) smart mailbox from text
  R("upsertSmart", name="xd quick", text="quick"), W(), P("smartTotal:xd quick", "smartNames"),
  R("selectSmart", text="xd quick"), W(), P(*st, "smartTotal:xd quick"), C("d-smart-selected"),
  # (e) search inside smart mailbox
  S("zebra"), P(*st, "subjects"), C("e-search-inside-smart"),
  S(""), P(*st, "subjects"),
  # (c) snoozed <-> smart
  R("selectSnoozed"), W(), P(*st), C("c-snoozed-after-smart"),
  R("selectSmart", text="xd quick"), W(), P(*st),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), P(*st),
  R("selectSnoozed"), W(), R("selectUnified", text="inbox"), W(), P(*st),
  R("selectSnoozed"), W(), R("selectSmart", text="xd quick"), W(), P(*st, "subjects"),
  # (f) delete selected smart mailbox
  R("deleteSmart", name="xd quick"), W(), P(*st, "smartNames"), C("f-after-delete-selected-smart"),
  S("quick"), P(*st, "subjects"), C("f-search-with-nothing-selected"), S(""), P(*st),
  # (j) odd smart mailbox queries
  R("upsertSmart", name="xd bogus", text="is:bogus before:garbage"), W(), R("selectSmart", text="xd bogus"), W(), P("messageCount", "smartTotal:xd bogus"),
  # (i) very long search text
  R("selectMailbox", accountRef="xd", text="inbox"), W(), S("quokka " * 800), P("messageCount", "errorText"), C("i-long-search"),
  S(""), P(*st),
  # (h) find similar with no AI configured
  R("selectMessage", text="xd phrase one"), W(), R("findSimilar"), W(), P("findSimilarShown", "findSimilarError"), C("h-find-similar-no-ai"),
  R("dismissFindSimilar"), W(),
]
write("xd-e2", ["explore"], steps, d=".")
