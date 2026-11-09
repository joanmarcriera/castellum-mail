# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
st = ["messageCount", "searchText", "mailboxRole", "smartSelected", "isSnoozedList"]
SH = "sheet:saveSearch"
def press(l): return {"op": "press", "window": SH, "find": {"role": "button", "label": l}}
def settext(l, t): return {"op": "setText", "window": SH, "find": {"role": "textField", "label": l}, "text": t}
steps = [XD(), PERS("alice"),
  R("selectMailbox", accountRef="xd", text="inbox"), W(),
  S(""),

  S(""),
  # snoozed + search + clear
  R("selectSnoozed"), W(), P(*st), S("quick"), P(*st), S(""), P(*st), C("snoozed-after-clearing-search"),
  # smart -> snoozed -> resync
  R("upsertSmart", name="xd quick", text="quick"), W(), R("selectSmart", text="xd quick"), W(), R("selectSnoozed"), W(), P(*st),
  R("reSyncAll"), W(), {"op": "wait", "until": "sync:xd=ok", "timeoutMs": 60000}, W(), P(*st), C("snoozed-after-resync"),
  # stale counts: unread smart mailbox
  R("selectMailbox", accountRef="xd", text="inbox"), W(),
  R("upsertSmart", name="xd unread fox", text="is:unread from:fox"), W(), R("selectSmart", text="xd unread fox"), W(), P("messageCount", "smartTotal:xd unread fox"),
  R("selectMessage", text="xd phrase one"), W(), R("waitFor", name="selectedUnread", text="false", timeoutMs=20000),
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("selectSmart", text="xd unread fox"), W(), P("messageCount", "smartTotal:xd unread fox"), C("unread-smart-after-reading"),
  # save search sheet
  R("selectMailbox", accountRef="xd", text="inbox"), W(), S("quick"),
  {"op": "openSheet", "target": "saveSearch"}, W("sheet:saveSearch"), W(), C("sheet-prefilled"),
  A(("reading:segment:" + SH, "Search"), window=SH, rules=["nonBlank", "fitsScreen", "noClippedText", "primaryButtonsVisible"]),
  settext("Name", "xd quick"), W(), press("Save"), W(), C("sheet-duplicate-name"), P("smartNames"),
  settext("Name", "   "), W(), C("sheet-blank-name"),
  settext("Name", "xd " + "verylongname " * 40), W(), press("Save"), W(), P("smartNames", "searchText", "messageCount"), C("after-long-name-save"),
]
write("xd-e3", ["explore"], steps, d=".")
