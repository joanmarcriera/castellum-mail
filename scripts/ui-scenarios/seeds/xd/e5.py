# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
st = ["messageCount", "searchText", "mailboxRole", "smartSelected", "isSnoozedList"]
SH = "sheet:saveSearch"
def settext(l, t): return {"op": "setText", "window": SH, "find": {"role": "textField", "label": l}, "text": t}
RET = {"op": "ext", "id": "keyEquivalent", "window": SH, "text": "return"}
OPEN = [{"op": "openSheet", "target": "saveSearch"}, W("sheet:saveSearch"), W()]
steps = [XD(), PERS("alice"), R("selectMailbox", accountRef="xd", text="inbox"), W(), R("searchScope", text="mailbox"), W()]
for q in ["subject:REUNIÓ", "subject:Año", "subject:AÑO", "from:NÚRIA", "from:\"núria solé\"", "from:\"Nuria Sole\"", "from:muñoz", "from:munoz", "from:Muñoz", "from:MUÑOZ", "Muñoz", "munoz"]:
    steps += [S(q), P("messageCount")]
steps += [S("quick"), OPEN, settext("Name", "xd saved"), W(), RET, W(), W("noSheet:saveSearch"), W(), P(*st, "smartNames", "smartTotal:xd saved"), C("after-save"),
  OPEN, settext("Name", "xd saved"), W(), RET, W(), C("duplicate-name-error"),
  A(("sheetAttached", "true")) if False else {"op": "assert", "expect": [{"sheetAttached": True}]},
  settext("Name", "xd " + "verylongname " * 40), W(), RET, W(), W("noSheet:saveSearch"), W(), P("smartCount"), C("after-long-name"),
  # tag search
  S(""), R("selectMessage", text="xd phrase one"), W(), R("createTag", name="xd tag", text="blue"), W(), P("tagNames"),
  R("toggleTag", text="xd tag"), W(), P("selectedKeywords", "tagNotice"),
  S("tag:\"xd tag\""), P("messageCount"), S("tag:xd"), P("messageCount"), S("tag:XD-TAG"), P("messageCount"), S(""),
  # delete the selected smart mailbox
  R("selectSmart", text="xd saved"), W(), R("deleteSmart", name="xd saved"), W(), P(*st), C("after-delete-selected-smart"),
  # find similar: AI on, endpoint dead, no embedding model
  R("selectMailbox", accountRef="xd", text="inbox"), W(), R("selectMessage", text="xd phrase one"), W(),
  {"op": "ext", "id": "setPref", "name": "aiEndpoint", "text": "http://127.0.0.1:1"}, {"op": "ext", "id": "setPref", "name": "aiModel", "text": "nomodel"},
  {"op": "ext", "id": "setPref", "name": "aiEnabled", "text": "true"},
  R("findSimilar"), W("sheet:findSimilar"), {"op": "ext", "id": "waitExt", "name": "similar.inFlight", "text": "false", "timeoutMs": 60000}, W(),
  P("findSimilarError"), C("find-similar-dead-endpoint"),
]
write("xd-e5", ["explore"], steps, d=".")
