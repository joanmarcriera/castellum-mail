# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer, second pass: snooze-return defect + tag probes."""
from mk import *
import json
def dump(*keys): return exp({k: "?" for k in keys})
P = lambda ms: E("pause", text=str(ms))
def smart(name, value): return R("upsertSmart", name=name, accountRef="xj", text=json.dumps({"op": "and", "conditions": [{"field": "tag", "operator": "is", "value": value}], "groups": []}))
write("xj-defect-snoozed-badge-stale-after-return", ["S43 snooze return", "Snoozed sidebar badge", "runSnoozeSweep reloads snoozedMessages only when the Snoozed list is open", "xj defect"], [
 start(), inbox(), sel("XJ Plain note"), R("remember"), R("snooze", text="3"), idle, wf("snoozedCount", "1"),
 exp({"reading:rememberedInList": "false"}),
 R("waitFor", name="rememberedInList", text="true", timeoutMs=50000), cap("message-back-in-inbox"),
 exp({"reading:messageCount": "10", "reading:rememberedSnoozed": "false", "reading:snoozedCount": "0"}),
], timeout=200000)
write("xj-probe-tags-local", ["probe"], [
 start(), inbox(), R("createTag", name="Feina", text="blue"), sel("XJ Plain note"), R("toggleTag", text="Feina"), idle, P(800),
 smart("Feina by name", "Feina"), R("reSyncAll"), P(8000), idle, R("selectSmart", text="Feina by name"), idle, dump("reading:subjects"),
 inbox(), R("createTag", name="Follow/up (2026) 100% \"x\"", text="green"), dump("reading:tagNames", "reading:errorText"),
 R("createTag", name="   ", text="green"), dump("reading:tagNames", "reading:errorText"),
])
write("xj-probe-tags-settings", ["probe"], [
 start(), inbox(), R("createTag", name="Feina", text="blue"), R("createTag", name="Casa", text="red"),
 sel("XJ Plain note"), R("toggleTag", text="Feina"), idle, P(800),
 {"op": "wait", "until": "settingsOpenerReady"}, R("settingsTab", tab="tags"), {"op": "wait", "until": "window:settings"}, idle, cap("tags-before", ["settings"]),
 A("click", window="settings", text="863,434"), P(900), dump("reading:tagNames"), cap("after-adopt", ["settings"]),
 A("click", window="settings", text="876,117"), P(900), cap("after-trash-click"), dump("reading:tagNames"),
 E("describe", window="settings"),
])
