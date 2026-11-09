# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer, second pass: probes (expected '?' dumps the real value in the report)."""
from mk import *
import json
A2 = "xj-org2@local.test"
def dump(*keys): return exp({k: "?" for k in keys})
P = lambda ms: E("pause", text=str(ms))
write("xj-probe-snooze-return", ["probe"], [
 start(), inbox(), sel("XJ Plain note"), R("remember"), R("snooze", text="3"), idle, wf("snoozedCount", "1"),
 dump("reading:messageCount", "reading:rememberedInList"),
 P(45000), cap("after-due-in-inbox"),
 dump("reading:snoozedCount", "reading:messageCount", "reading:rememberedInList", "reading:rememberedSnoozed", "reading:subjects"),
 sel("XJ Server keywords"), R("remember"), R("snooze", text="3"), idle, wf("snoozedCount", "1", 5000),
 R("selectSnoozed"), idle, dump("reading:snoozedCount", "reading:messageCount"), sel("XJ Server keywords"),
 P(40000), cap("after-due-in-snoozed-view"),
 dump("reading:snoozedCount", "reading:messageCount", "reading:selectedSubject", "reading:isSnoozedList"),
 inbox(), dump("reading:messageCount", "reading:rememberedInList"),
], timeout=240000)
write("xj-probe-tags-writes", ["probe"], [
 start(A2), inbox(), R("createTag", name="Feina", text="blue"),
 sel("XJ Plain note"), R("toggleTag", text="Feina"), idle, P(1200),
 dump("reading:tagNotice", "ext:pendingCount", "reading:selectedKeywords", "reading:errorText"), cap("eval-mode-tag"),
 E("enableWrites", email=A2), P(500),
 sel("XJ Roadmap thread"), R("toggleTag", text="Feina"), idle, P(4000),
 dump("reading:tagNotice", "ext:pendingCount", "reading:selectedKeywords", "reading:errorText"),
 sel("XJ Club news (mailto only)"), R("toggleTag", text="Feina"), idle, P(3000), R("toggleTag", text="Feina"), idle, P(4000),
 dump("reading:tagNotice", "ext:pendingCount", "reading:selectedKeywords", "reading:errorText"),
 sel("XJ Plain note"), dump("reading:selectedKeywords"),
 R("reSyncAll"), P(6000), idle, inbox(), sel("XJ Plain note"), dump("reading:selectedKeywords", "reading:tagDots:0"), cap("after-resync"),
 R("createTag", name="feina", text="red"), dump("reading:tagNames"),
 R("createTag", name="Follow/up (2026) 100% \"x\"", text="green"), dump("reading:tagNames"),
 R("createTag", name="   ", text="green"), dump("reading:tagNames", "reading:errorText"),
])
