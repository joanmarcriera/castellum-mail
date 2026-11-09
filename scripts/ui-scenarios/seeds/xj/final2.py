# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer, second pass: kept scenarios (3 new defects, 2 repaired pass scenarios, 4 new pass scenarios)."""
from mk import *
import json
A2 = "xj-org2@local.test"
P = lambda ms: E("pause", text=str(ms))
EVAL = "evaluation mode: no changes are made to the mail server (enable writes to act)"
NOTICE = "Tag kept on this Mac only: this account is in evaluation mode."
def smart(name, value): return R("upsertSmart", name=name, accountRef="xj", text=json.dumps({"op": "and", "conditions": [{"field": "tag", "operator": "is", "value": value}], "groups": []}))
def link(tag): return [A("click", window="main", text="928,145", name="sheet:" + tag), P(700), {"op": "wait", "until": "sheet:" + tag}]

# ---------------- defects ----------------
write("xj-defect-oneclick-unsubscribe-ignores-evaluation-mode", ["S44 one-click unsubscribe", "evaluation mode", "unsubscribe_one_click_json has no write-policy check", "xj defect"], [
 start(), inbox(), sel("XJ Weekly digest (one-click)"), link("oc"), cap("confirm-sheet"),
 A("click", window="sheet:oc", text="310,141"), P(4000), cap("request-was-attempted"),
 exp({"reading:errorText": EVAL}),
])
write("xj-defect-local-tag-removal-undone-by-sync", ["S42 tags", "evaluation mode local-only notice", "xj defect"], [
 start(), inbox(), {"op": "search", "text": "tag:xjServerTag"}, idle, sel("XJ Server keywords"),
 R("toggleTag", text="xjServerTag"), idle, P(1500), exp({"reading:selectedKeywords": "", "reading:tagNotice": NOTICE}),
 {"op": "search", "text": ""}, idle, {"op": "search", "text": "tag:xjServerTag"}, idle, exp({"reading:subjects": ""}), cap("removed-on-this-mac"),
 {"op": "search", "text": ""}, idle, R("reSyncAll"), P(8000), idle,
 {"op": "search", "text": "tag:xjServerTag"}, idle, cap("back-after-sync"), exp({"reading:subjects": ""}),
])

# ---------------- repaired pass scenarios (same names as the first pass) ----------------
write("xj-unsubscribe-sheet-destination-and-cancel", ["S44 List-Unsubscribe", "confirmation shows the exact destination", "cancel sends nothing", "xj"], [
 start(), inbox(), sel("XJ Club news (mailto only)"), link("mailto"), cap("mailto-sheet"),
 {"op": "assert", "window": "sheet:mailto", "rules": ["nonBlank", "fitsScreen"], "expect": [{"find": {"role": "staticText", "label": "xj-unsub@local.test"}, "exists": True}]},
 A("click", window="sheet:mailto", text="216,117"), {"op": "wait", "until": "noSheet:mailto"},
 exp({"ext:pendingCount": "0", "ext:drafts:" + ACCT: "", "reading:errorText": "none"}, finds=[{"sheetAttached": False}]),
 sel("XJ Weekly digest (one-click)"), link("oneclick"), cap("one-click-sheet"),
 {"op": "assert", "window": "sheet:oneclick", "rules": ["nonBlank", "fitsScreen"], "expect": [{"find": {"role": "staticText", "label": "https://127.0.0.1:9/unsub?u=xj"}, "exists": True}]},
 A("click", window="sheet:oneclick", text="216,141"), {"op": "wait", "until": "noSheet:oneclick"}, P(1500),
 exp({"ext:pendingCount": "0", "reading:errorText": "none"}, finds=[{"sheetAttached": False}]),
])
write("xj-tags-settings-long-name-and-server-keywords", ["S42 Settings > Tags", "long tag name", "adopt a server keyword", "protocol keywords are not offered", "xj"], [
 start(), inbox(), R("createTag", name="A very long tag name that a person might type when they treat tags like notes to themselves about next week", text="green"),
 R("createTag", name="Feina urgent", text="blue"),
 {"op": "wait", "until": "settingsOpenerReady"}, R("settingsTab", tab="tags"), {"op": "wait", "until": "window:settings"}, idle,
 cap("settings-tags", ["settings"]),
 {"op": "assert", "window": "settings", "rules": ["nonBlank", "fitsScreen"]},
 A("click", window="settings", text="863,434"), wf("tagNames", "A very long tag name that a person might type when they treat tags like notes to themselves about next week,Feina urgent,xjServerTag", 8000),
 cap("after-adopting-the-only-offered-keyword", ["settings"]),
 {"op": "assert", "window": "settings", "rules": ["nonBlank", "fitsScreen"]},
])

# ---------------- new pass scenarios ----------------
write("xj-snooze-due-while-snoozed-list-open", ["S43 snooze return", "due sweep", "xj"], [
 start(), inbox(), sel("XJ Plain note"), R("remember"), R("snooze", text="3"), idle, wf("snoozedCount", "1"),
 R("selectSnoozed"), idle, exp({"reading:isSnoozedList": "true"}),
 R("waitFor", name="snoozedCount", text="0", timeoutMs=50000), exp({"reading:messageCount": "0"}), cap("snoozed-list-emptied-when-due"),
 inbox(), exp({"reading:rememberedInList": "true", "reading:messageCount": "10"}, window="main", rules=["nonBlank"]), cap("back-in-inbox"),
], timeout=200000)
write("xj-tags-evaluation-mode-local-only", ["S42 tags", "evaluation mode keeps tags local", "local tag survives a re-sync", "xj"], [
 start(), inbox(), R("createTag", name="Feina", text="blue"), sel("XJ Plain note"), R("toggleTag", text="Feina"), idle,
 wf("tagNotice", NOTICE, 8000), exp({"ext:pendingCount": "0", "reading:selectedKeywords": "Feina", "reading:errorText": "none"}), cap("local-only-notice"),
 smart("Feina by name", "Feina"), R("reSyncAll"), P(8000), idle, R("selectSmart", text="Feina by name"), idle,
 exp({"reading:subjects": "XJ Plain note", "ext:pendingCount": "0"}, window="main", rules=["nonBlank"]),
 sel("XJ Plain note"), R("toggleTag", text="Feina"), idle, P(1500), R("selectSmart", text="Feina by name"), idle, exp({"reading:messageCount": "0"}),
])
write("xj-tags-writes-enabled-add-remove", ["S42 tags", "writes enabled: keyword reaches the server and is removed again", "xj"], [
 start(A2), E("enableWrites", email=A2), inbox(), R("createTag", name="Casa", text="red"),
 sel("XJ Club news (mailto only)"), R("toggleTag", text="Casa"), idle, wf("selectedKeywords", "Casa", 8000), P(3000),
 exp({"reading:tagNotice": "none", "reading:errorText": "none", "ext:pendingCount": "0"}), cap("tagged-with-writes"),
 {"op": "search", "text": "tag:Casa"}, idle, exp({"reading:subjects": "XJ Club news (mailto only)"}),
 sel("XJ Club news (mailto only)"), R("toggleTag", text="Casa"), idle, P(3000),
 {"op": "search", "text": ""}, idle, {"op": "search", "text": "tag:Casa"}, idle,
 exp({"reading:subjects": "", "reading:errorText": "none", "ext:pendingCount": "0"}),
])
write("xj-organise-in-unified-inbox-two-accounts", ["unified inbox", "snooze and mute with two accounts", "switch mailbox mid-way", "xj"], [
 start(), {"op": "addAccount", "email": A2, "password": A2, "as": "x2", "waitFor": "synced", "timeoutMs": 90000},
 R("selectUnified", text="inbox"), idle, exp({"reading:isUnified": "true", "reading:messageCount": "20"}),
 sel("XJ Plain note"), R("remember"), R("snooze", text="3600"), idle, wf("snoozedCount", "1"),
 exp({"reading:messageCount": "19", "reading:rememberedInList": "false"}), cap("unified-after-snooze"),
 sel("XJ Roadmap thread"), R("toggleMute"), idle, wf("selectedMuted", "true"),
 inbox("x2"), exp({"reading:mailboxRole": "inbox"}), R("selectSnoozed"), idle, exp({"reading:messageCount": "1", "reading:rememberedInList": "true"}),
 sel("XJ Plain note"), R("unsnooze"), idle, wf("snoozedCount", "0"),
 R("selectUnified", text="inbox"), idle, exp({"reading:messageCount": "20", "reading:rememberedInList": "true"}, window="main", rules=["nonBlank"]), cap("unified-restored"),
])
