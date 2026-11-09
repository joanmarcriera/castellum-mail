# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer: writes the kept scenarios (xj-*.json pass, xj-defect-*.json fail on a real defect)."""
from mk import *
import json
def smart(name, value): return R("upsertSmart", name=name, accountRef="xj", text=json.dumps({"op": "and", "conditions": [{"field": "tag", "operator": "is", "value": value}], "groups": []}))
LINK = "928,145"   # reading-pane "Unsubscribe" link in the default 1297x450 Classic layout (read off the PNG)
def link(tag): return [A("click", window="main", text=LINK, name="sheet:" + tag), E("pause", text="700")]

# ---------------- defects ----------------
write("xj-defect-tags-vanish-on-mailbox-reload", ["S42 tags", "list_message_page never calls attach_keywords", "xj defect"], [
 start(), inbox(), sel("XJ Plain note"), R("createTag", name="Feina", text="blue"),
 R("toggleTag", text="Feina"), idle, wf("selectedKeywords", "Feina"), E("pause", text="600"),
 cap("tagged-dot-visible"), exp({"reading:selectedKeywords": "Feina", "reading:tagDots:0": "●"}),
 inbox(), sel("XJ Plain note"), cap("after-reopening-the-inbox"),
 exp({"reading:selectedKeywords": "Feina", "reading:tagDots:0": "●"}),
 sel("XJ Server keywords"), exp({"reading:selectedKeywords": "xjServerTag"}),
])
write("xj-defect-user-tag-named-junk-invisible", ["S42 tags", "MM1c hidden keywords", "xj defect"], [
 start(), inbox(), sel("XJ Plain note"), R("createTag", name="Junk", text="red"),
 exp({"reading:tagNames": "Junk"}),
 R("toggleTag", text="Junk"), idle, E("pause", text="600"), cap("junk-dot-right-after-tagging"),
 smart("Any tag", True), R("selectSmart", text="Any tag"), idle, cap("has-any-tag-smart-mailbox"),
 exp({"reading:hasSubject:XJ Plain note": "true"}),
 {"op": "search", "text": "tag:Junk"}, idle, sel("XJ Plain note"), cap("tag-search-result"),
 exp({"reading:selectedKeywords": "Junk", "reading:tagDots:0": "●"}),
])
write("xj-defect-unsubscribe-https-only-dead-end", ["S44 List-Unsubscribe", "RFC 2369 https link without List-Unsubscribe-Post", "xj defect"], [
 start(), inbox(), sel("XJ Shop offers (https only)"), cap("unsubscribe-link-offered"),
 link("unsub"), {"op": "wait", "until": "sheet:unsub"}, cap("sheet-says-no-link"),
 {"op": "assert", "window": "sheet:unsub", "rules": ["nonBlank"], "expect": [{"find": {"role": "staticText"}, "exists": True}]},
])
write("xj-defect-unsubscribe-refused-leaves-draft", ["S44 List-Unsubscribe mailto", "evaluation mode", "xj defect"], [
 start(), inbox(), sel("XJ Club news (mailto only)"),
 link("unsub"), {"op": "wait", "until": "sheet:unsub"}, cap("confirm-sheet"),
 A("click", window="sheet:unsub", text="310,117"), E("pause", text="2000"), cap("refused"),
 E("openFirstDraft", email=ACCT, timeoutMs=3000), E("pause", text="600"), cap("the-draft-left-behind"),
 exp({"ext:pendingCount": "0", "ext:composeWindows": "0", "ext:drafts:" + ACCT: ""}),
])

# ---------------- passing coverage ----------------
write("xj-mute-thread-survives-reload", ["S42 mute thread", "xj"], [
 start(), inbox(), sel("XJ Roadmap thread"), R("toggleMute"), idle, wf("selectedMuted", "true"),
 inbox(), sel("Re: XJ Roadmap thread"), exp({"reading:selectedMuted": "true"}),
 sel("XJ Roadmap thread"), exp({"reading:selectedMuted": "true"}), cap("whole-thread-muted-after-reload"),
 sel("XJ Plain note"), exp({"reading:selectedMuted": "false"}),
 sel("XJ Roadmap thread"), R("toggleMute"), idle, wf("selectedMuted", "false"),
 inbox(), sel("Re: XJ Roadmap thread"), exp({"reading:selectedMuted": "false"}, window="main", rules=["nonBlank"]),
])
write("xj-snooze-resnooze-unsnooze", ["S43 snooze", "re-snooze inside Snoozed", "back to inbox", "xj"], [
 start(), inbox(), sel("XJ Plain note"), R("remember"), R("snooze", text="3600"), idle, wf("rememberedInList", "false"),
 wf("snoozedCount", "1"), exp({"reading:messageCount": "9", "reading:selectedSubject": "none"}),
 R("selectSnoozed"), idle, exp({"reading:isSnoozedList": "true", "reading:subjects": "XJ Plain note"}), cap("snoozed-list"),
 sel("XJ Plain note"), R("snooze", text="7200"), idle, E("pause", text="800"),
 R("selectSnoozed"), idle, exp({"reading:snoozedCount": "1", "reading:messageCount": "1"}), cap("re-snoozed-still-one-row"),
 sel("XJ Plain note"), R("unsnooze"), idle, wf("snoozedCount", "0"), exp({"reading:messageCount": "0"}), cap("snoozed-empty"),
 inbox(), exp({"reading:rememberedInList": "true", "reading:messageCount": "10"}, window="main", rules=["nonBlank"]),
])
write("xj-tag-smart-mailbox-conditions", ["MM1b tag conditions", "MM1c hidden server keywords", "xj"], [
 start(), inbox(), R("createTag", name="Feina urgent", text="blue"),
 sel("XJ Plain note"), R("toggleTag", text="Feina_urgent"), idle, E("pause", text="600"),
 smart("Any tag", True), smart("No tag", False), smart("Feina by name", "Feina urgent"), smart("Server tag", "xjServerTag"), smart("Hidden kw", "$Forwarded"),
 R("selectSmart", text="Any tag"), idle, exp({"reading:subjects": "XJ Plain note | XJ Server keywords"}), cap("any-tag"),
 R("selectSmart", text="No tag"), idle, exp({"reading:messageCount": "8"}),
 R("selectSmart", text="Feina by name"), idle, exp({"reading:subjects": "XJ Plain note", "reading:tagDots:0": "●"}),
 R("selectSmart", text="Server tag"), idle, exp({"reading:subjects": "XJ Server keywords"}),
 sel("XJ Server keywords"), exp({"reading:selectedKeywords": "xjServerTag"}, window="main", rules=["nonBlank"]), cap("server-keyword-without-hidden-ones"),
])
write("xj-correspondence-thread-and-empty", ["MM-4 correspondence", "empty state", "xj"], [
 start(), inbox(), sel("XJ Roadmap thread"), R("correspondence", text="pau@partner.example.test"), idle,
 exp({"reading:correspondence": "pau@partner.example.test", "reading:corrTotal": "4", "reading:corrReceived": "4", "reading:corrSent": "0", "reading:messageCount": "4"}),
 cap("correspondence-with-pau"),
 R("correspondence", text="nobody at all"), idle, exp({"reading:corrTotal": "0", "reading:messageCount": "0"}), cap("correspondence-empty-state"),
 R("closeCorrespondence"), idle,
 exp({"reading:correspondence": "none", "reading:mailboxRole": "inbox", "reading:messageCount": "10"}, window="main", rules=["nonBlank"]),
])
write("xj-unsubscribe-sheet-destination-and-cancel", ["S44 List-Unsubscribe", "confirmation shows the exact destination", "cancel", "http-only offers nothing", "xj"], [
 start(), inbox(), sel("XJ Club news (mailto only)"), link("mailto"), {"op": "wait", "until": "sheet:mailto"}, cap("mailto-sheet"),
 {"op": "assert", "window": "sheet:mailto", "rules": ["nonBlank", "fitsScreen"], "expect": [{"find": {"role": "staticText", "label": "xj-unsub@local.test"}, "exists": True}]},
 A("click", window="sheet:mailto", text="216,117"), {"op": "wait", "until": "noSheet:mailto"},
 exp({"ext:pendingCount": "0", "ext:drafts:" + ACCT: "", "reading:errorText": "none"}, finds=[{"sheetAttached": False}]),
 sel("XJ Legacy list (http only)"), link("http"), cap("http-only-no-link"),
 exp({"reading:errorText": "none"}, finds=[{"sheetAttached": False}]),
 sel("XJ Weekly digest (one-click)"), link("oneclick"), {"op": "wait", "until": "sheet:oneclick"}, cap("one-click-sheet"),
 {"op": "assert", "window": "sheet:oneclick", "rules": ["nonBlank", "fitsScreen"], "expect": [{"find": {"role": "staticText", "label": "https://127.0.0.1:9/unsub?u=xj"}, "exists": True}]},
])
LONG = "Projecte amb un nom molt llarg per veure com es comporta el tauler quan algu escriu una frase sencera com a titol"
write("xj-projects-board-long-name", ["Active Work project board", "project link provenance", "smallest window", "xj"], [
 start(), inbox(), sel("XJ Plain note"), R("createProjectAssign", name=LONG), idle,
 exp({"reading:projectCount": "1"}), R("projectLinks"), exp({"reading:noteHas:derived|message:": "true"}),
 {"op": "openWindow", "target": "projects"}, {"op": "wait", "until": "window:projects"}, idle, cap("board", ["projects"]),
 {"op": "resizeWindow", "window": "projects", "contentSize": "min"}, idle, cap("board-min", ["projects"]),
 {"op": "assert", "window": "projects", "rules": ["nonBlank", "fitsScreen", "noClippedText", "controlsReachable", "primaryButtonsVisible"]},
])
write("xj-tags-settings-long-name-and-server-keywords", ["S42 Settings > Tags", "long tag name", "adopt list hides protocol keywords", "xj"], [
 start(), inbox(), R("createTag", name="A very long tag name that a person might type when they treat tags like notes to themselves about next week", text="green"),
 R("createTag", name="Feina urgent", text="blue"),
 {"op": "wait", "until": "settingsOpenerReady"}, R("settingsTab", tab="tags"), {"op": "wait", "until": "window:settings"}, idle,
 cap("settings-tags", ["settings"]),
 {"op": "assert", "window": "settings", "rules": ["nonBlank", "fitsScreen"], "expect": [
   {"find": {"role": "button", "label": "Add"}, "exists": True},
   {"find": {"text": "Forwarded"}, "exists": False}, {"find": {"text": "NonJunk"}, "exists": False}]},
])
