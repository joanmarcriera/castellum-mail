# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Generates the xd scenarios (defects + coverage) into scripts/ui-scenarios/. Mail fixture: seed-xd.py (xd-s1@local.test)."""
from gen import *
import unicodedata
SH = "sheet:saveSearch"
def settext(l, t): return {"op": "setText", "window": SH, "find": {"role": "textField", "label": l}, "text": t}
RET = {"op": "ext", "id": "keyEquivalent", "window": SH, "text": "return"}
OPEN = [{"op": "openSheet", "target": "saveSearch"}, W("sheet:saveSearch"), W()]
def XI(scope="mailbox"): return [XD(), R("selectMailbox", accountRef="xd", text="inbox"), W(), R("searchScope", text=scope), W()]
def n(q, c): return [S(q), A(("reading:messageCount", str(c)), ("reading:searchText", q))]
def has(q, subj, c=1): return [S(q), A(("reading:messageCount", str(c)), ("reading:hasSubject:" + subj, "true"))]
NOERR = A(("reading:errorText", "none"))

# ---------------- defects ----------------
write("xd-defect-quoted-phrase-not-exact", ["search", "S39 grammar: \"exact phrase\""], XI() + [
  S("\"quick brown fox\""), C("quoted-phrase-returns-non-phrase-mail"),
  A(("reading:hasSubject:xd phrase one", "true"), ("reading:hasSubject:xd phrase two", "false"), ("reading:messageCount", "1")),
  S("\"brown quick\""), A(("reading:messageCount", "0")),
  S("body:\"brown quick\""), A(("reading:messageCount", "0")),
  S("-\"lazy dog\" fox"), A(("reading:hasSubject:xd phrase two", "true")),
  OPEN, C("save-sheet-promises-exact-phrase")])

steps = XI()
for q in ["reunio", "REUNIÓ", "subject:reunió", "subject:reunio", "subject:REUNIÓ", "subject:AÑO", "subject:ano",
          "munoz", "from:Muñoz", "from:munoz", "from:MUÑOZ", "from:NÚRIA", "from:\"Nuria Sole\""]:
    steps += n(q, 1)
steps += [S("subject:REUNIÓ"), C("subject-qualifier-uppercase-accent-finds-nothing")]
write("xd-defect-qualifier-accent-case-sensitive", ["search", "accents ca/es", "from: subject:"], steps)

write("xd-defect-stale-search-text-after-mailbox-switch", ["search then switch mailbox"], [XD(), PERS("alice"),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("searchScope", text="mailbox"), W(),
  S("quick"), A(("reading:messageCount", "2")),
  R("selectMailbox", accountRef="alice", text="inbox"), W(), C("field-says-quick-list-is-whole-inbox"),
  A(("reading:mailboxRole", "inbox"), ("reading:hasSubject:Invoice R0012345678", "true")),
  # the list is the unfiltered mailbox, so the field must not still claim a search is applied
  A(("reading:searchText", "")),
  # same with a smart mailbox: save the search, click the new smart mailbox, the field still says "quick"
  S(""), R("selectMailbox", accountRef="xd", text="inbox"), W(), S("quick"), R("upsertSmart", name="xd other", text="zebra"), W(),
  R("selectSmart", text="xd other"), W(), C("smart-mailbox-zebra-open-field-says-quick"),
  A(("reading:hasSubject:Re: \"Budget\" v2: final", "true"), ("reading:messageCount", "1"), ("reading:searchText", ""))])

write("xd-defect-late-search-result-overwrites-mailbox", ["search then switch mailbox", "runSearch has no listToken guard"], [XD(), PERS("alice"),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("searchScope", text="all"), W(),
  {"op": "search", "text": "is:unread"},            # Return on a cross-account search ...
  R("selectMailbox", accountRef="xd", text="inbox"), # ... and click a mailbox before it lands
  W(), C("xd-inbox-selected-but-list-is-search-result"),
  A(("reading:mailboxRole", "inbox"), ("reading:messageCount", "10"), ("reading:hasSubject:Invoice R0012345678", "false"))])

write("xd-defect-snoozed-smart-selection-leak", ["smart mailboxes", "Snoozed", "sidebar selection"], [XD(),
  R("selectMailbox", accountRef="xd", text="inbox"), W(),
  R("upsertSmart", name="xd quick", text="quick"), W(),
  R("selectSmart", text="xd quick"), W(), A(("reading:messageCount", "2")),
  R("selectSnoozed"), W(),
  A(("reading:isSnoozedList", "true"), ("reading:messageCount", "0"), ("reading:smartSelected", "none")),
  R("reSyncAll"), W(), {"op": "wait", "until": "sync:xd=ok", "timeoutMs": 60000}, W(), C("snoozed-list-replaced-by-smart-mailbox-after-sync"),
  A(("reading:isSnoozedList", "true"), ("reading:messageCount", "0")),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("selectSnoozed"), W(), R("selectSmart", text="xd quick"), W(),
  A(("reading:smartSelected", "xd quick"), ("reading:isSnoozedList", "false"))])

write("xd-defect-smart-count-stale-after-read", ["smart mailboxes", "counts sidebar vs list"], [XD(),
  R("selectMailbox", accountRef="xd", text="inbox"), W(),
  R("upsertSmart", name="xd unread fox", text="is:unread from:fox"), W(), R("selectSmart", text="xd unread fox"), W(),
  A(("reading:messageCount", "2"), ("reading:smartTotal:xd unread fox", "2")),
  R("selectMessage", text="xd phrase one"), W(), R("waitFor", name="selectedUnread", text="false", timeoutMs=20000),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("selectSmart", text="xd unread fox"), W(), C("list-has-1-sidebar-count-still-2"),
  A(("reading:messageCount", "1"), ("reading:smartTotal:xd unread fox", "1"))])

write("xd-defect-search-in-snoozed-view", ["search", "Snoozed", "clearing the search"], [XD(), PERS("alice"),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("searchScope", text="mailbox"), W(),
  R("selectMessage", text="xd cc test"), W(), R("remember"), R("snooze", text="3600"), W(),
  R("waitFor", name="rememberedInList", text="false", timeoutMs=15000),
  R("selectSnoozed"), W(), A(("reading:isSnoozedList", "true"), ("reading:hasSubject:xd cc test", "true"), ("reading:messageCount", "1")),
  # 1. a search typed in Snoozed silently searches only the first account (alice): xd's mail is not found
  S("quick"), C("search-from-snoozed-finds-nothing"), A(("reading:hasSubject:xd phrase one", "true")),
  # 2. clearing the field must bring the Snoozed list back
  R("searchScope", text="all"), S("zebra"), A(("reading:hasSubject:Re: \"Budget\" v2: final", "true")),
  S(""), C("cleared-search-still-shows-results-in-snoozed"),
  A(("reading:searchText", ""), ("reading:isSnoozedList", "true"), ("reading:hasSubject:xd cc test", "true"), ("reading:hasSubject:Re: \"Budget\" v2: final", "false"))])

write("xd-defect-nfd-query-finds-nothing", ["search", "accents ca/es", "Unicode normalisation"], XI() +
  n("càlcul", 1) + n(unicodedata.normalize("NFD", "càlcul"), 1) + n(unicodedata.normalize("NFD", "año"), 1) + [C("nfd-query")])

# ---------------- coverage (must pass) ----------------
steps = XI() + [A(("reading:messageCount", "10"))]
for q, s in [("pressupost", "Reunió de pressupost"), ("PRESSUPOST", "Reunió de pressupost"), ("calcul", "Reunió"), ("càlcul", "Reunió"), ("CÀLCUL", "Reunió"),
             ("matí", "Reunió"), ("mati", "Reunió"), ("d'enginyeria", "Reunió"), ("año", "Presupuesto año"), ("ano", "Presupuesto año"), ("nino", "Presupuesto año"),
             ("señor", "Presupuesto año"), ("Núria", "Reunió"), ("nuria", "Reunió"), ("C++", "C++ and 50%"), ("50%", "C++ and 50%"), ("(limited)", "C++ and 50%"),
             ("[promo]", "C++ and 50%"), ("pangolin-report", "xd report with attachment"), ("superlongsubjectword", "xd long")]:
    steps += has(q, s)
steps += [C("accent-search-result")] + n("zzzznotfound", 0) + [C("no-results"), NOERR, S(""), A(("reading:messageCount", "10"), ("reading:searchText", ""), ("reading:mailboxRole", "inbox")), C("cleared")]
write("xd-search-words-accents", ["search: plain words, accents, no results, clear"], steps)

steps = XI()
for q, s, c in [("from:nuria", "Reunió", 1), ("from:\"Núria Solé\"", "Reunió", 1), ("from:fox@xd.example", "xd phrase one", 2), ("from:=fox@xd.example", "xd phrase two", 2),
                ("to:other@xd.example", "xd cc test", 1), ("subject:C++", "C++ and 50%", 1), ("subject:\"\\\"Budget\\\" v2\"", "Re: \"Budget\" v2: final", 1),
                ("subject:\"phrase one\"", "xd phrase one", 1), ("has:attachment", "xd report with attachment", 1), ("HAS:ATTACHMENT", "xd report", 1),
                ("attachment:pangolin", "xd report", 1), ("filename:pangolin-report.pdf", "xd report", 1),
                ("before:2021-01-01", "xd old message from 2020", 1), ("before:2020-01-16", "xd old message", 1), ("after:2020-01-15 before:2020-01-16", "xd old message", 1),
                ("date:2020", "xd old message", 1), ("date:2020-01", "xd old message", 1), ("in:inbox", "xd phrase one", 10), ("in:Inbox", "xd phrase one", 10),
                ("is:unread", "xd phrase one", 10), ("is:UNREAD", "xd phrase one", 10), ("-quick", "xd cc test", 8), ("-lazy quick", "xd phrase two", 1),
                ("quick   brown", "xd phrase one", 2), ("within:30d", "xd cc test", 9), ("size:>1", "xd cc test", 10)]:
    steps += has(q, s, c)
for q in ["subject:\"one phrase\"", "before:2020-01-15", "date:2020-02", "in:sent", "in:trash", "is:read", "is:flagged", "tag:work", "after:2021-01-01 ocelot", "is:unread is:read", "size:<10"]:
    steps += n(q, 0)
steps += [NOERR, C("qualifiers-last")]
write("xd-search-qualifiers", ["search: from: to: subject: has: before: after: date: in: is: tag: -exclude"], steps)

steps = XI()
for q in ["\"unterminated quote", "'", "\"\"", "*", "NOT", "OR quick", "before:garbage", "before:15/01/2020", "has:attachments", "tag:", "from:", "-", ":", "quokka " * 800,
          "x" * 4000, "subject:" + "y" * 3000, "💥 emoji", "%_%", "\\", "a\tb", "DROP TABLE messages; --"]:
    steps += [S(q), A(("reading:searchText", q), ("reading:errorText", "none"), ("reading:mailboxRole", "inbox"))]
steps += [C("hostile-last"), S(""), A(("reading:messageCount", "10"))]
write("xd-search-hostile-input", ["search: wrong input, very long text, never errors"], steps)

long_name = "xd " + "verylongname " * 40
write("xd-save-search-sheet", ["saving a search as a smart mailbox", "duplicate name", "very long name"], XI() + [
  S("quick"), OPEN, C("prefilled"),
  A(("reading:segment:" + SH, "Search"), window=SH, rules=["nonBlank", "fitsScreen", "noZeroSizeTextFields", "noClippedText"]),
  settext("Name", "xd saved"), W(), RET, W("noSheet:saveSearch"), W(),
  A(("reading:smartNames", "xd saved"), ("reading:smartTotal:xd saved", "2"), ("reading:searchText", "quick"), ("reading:messageCount", "2")),
  OPEN, settext("Name", "xd saved"), W(), RET, W(), C("duplicate-name-error"),
  {"op": "assert", "window": SH, "rules": ["nonBlank", "fitsScreen"], "expect": [{"sheetAttached": True}, {"model": "reading:smartCount", "equals": "1"}]},
  settext("Name", long_name), W(), RET, W("noSheet:saveSearch"), W(), C("after-long-name"),
  A(("reading:smartCount", "2")),
  R("selectSmart", text="xd saved"), W(), A(("reading:smartSelected", "xd saved"), ("reading:messageCount", "2"))])

write("xd-smart-mailbox-lifecycle", ["smart mailboxes: create, select, search inside, clear, delete"], XI() + [
  R("upsertSmart", name="xd quick", text="quick"), W(), R("upsertSmart", name="xd bogus", text="is:bogus before:garbage"), W(),
  A(("reading:smartNames", "xd bogus,xd quick"), ("reading:smartTotal:xd quick", "2"), ("reading:smartTotal:xd bogus", "0")),
  R("selectSmart", text="xd quick"), W(), A(("reading:smartSelected", "xd quick"), ("reading:messageCount", "2"), ("reading:mailboxRole", "none")), C("smart-selected"),
  S("zebra"), A(("reading:messageCount", "1"), ("reading:smartSelected", "xd quick")),
  S(""), A(("reading:messageCount", "2"), ("reading:hasSubject:xd phrase one", "true")),
  R("selectSmart", text="xd bogus"), W(), A(("reading:messageCount", "0"), ("reading:errorText", "none")), C("smart-empty"),
  R("deleteSmart", name="xd quick"), W(), A(("reading:smartNames", "xd bogus"), ("reading:smartSelected", "xd bogus")),
  R("deleteSmart", name="xd bogus"), W(), A(("reading:smartCount", "0"), ("reading:smartSelected", "none"), ("reading:messageCount", "0")), C("deleted-while-selected"),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), A(("reading:messageCount", "10"))])

write("xd-tag-search", ["search: tag:"], XI() + [
  R("selectMessage", text="xd phrase one"), W(), R("createTag", name="xd tag", text="blue"), W(), R("toggleTag", text="xd tag"), W(),
  R("waitFor", name="selectedKeywords", text="xd tag", timeoutMs=15000), W(),
  S("tag:\"xd tag\""), A(("reading:messageCount", "1"), ("reading:hasSubject:xd phrase one", "true")), C("tag-search"),
  S("-tag:\"xd tag\""), A(("reading:messageCount", "9")), S("tag:nosuchtag"), A(("reading:messageCount", "0")), S(""), A(("reading:messageCount", "10"))])

write("xd-find-similar-no-model", ["Find Similar without an embedding model"], XI() + [
  R("selectMessage", text="xd phrase one"), W(), R("findSimilar"), W("sheet:findSimilar"), W(), C("ai-off"),
  A(("reading:findSimilarShown", "true"), ("reading:findSimilarError", "Set an Ollama endpoint and model in Settings → AI first."), window="sheet:findSimilar", rules=["nonBlank", "fitsScreen"]),
  R("dismissFindSimilar"), W("noSheet:findSimilar"), A(("reading:findSimilarShown", "false"), ("reading:selectedSubject", "xd phrase one")),
  A(("reading:messageCount", "10"), ("reading:mailboxRole", "inbox"))])

write("xd-search-scope-two-accounts", ["search scope: mailbox / account / all", "searching then switching scope"], [XD(), PERS("alice"),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("searchScope", text="mailbox"), W(),
  S("subject:invoice"), A(("reading:messageCount", "0")),
  R("searchScope", text="account"), W(), A(("reading:messageCount", "0")),
  R("searchScope", text="all"), W(), A(("reading:hasSubject:Invoice R0012345678", "true")), C("scope-all"),
  S("quick"), A(("reading:messageCount", "2")),
  R("selectUnified", text="inbox"), W(), R("searchScope", text="mailbox"), W(), S("quick"), A(("reading:messageCount", "2"), ("reading:isUnified", "true")),
  S(""), A(("reading:isUnified", "true"), ("reading:hasSubject:xd phrase one", "true"), ("reading:hasSubject:Invoice R0012345678", "true"))])

PA = lambda ms: {"op": "ext", "id": "pause", "text": str(ms)}
write("xd-defect-find-similar-second-open", ["Find Similar", "do it twice"], XI() + [
  R("selectMessage", text="xd phrase one"), W(), R("findSimilar"), W("sheet:findSimilar"), W(), C("first-open"),
  {"op": "assert", "expect": [{"model": "reading:findSimilarShown", "equals": "true"}, {"sheetAttached": True}]},
  R("dismissFindSimilar"), W("noSheet:findSimilar"), PA(3000),
  R("findSimilar"), PA(1500), W(), C("second-open-no-sheet"),
  {"op": "assert", "expect": [{"model": "reading:findSimilarShown", "equals": "true"}, {"sheetAttached": True}]}])
