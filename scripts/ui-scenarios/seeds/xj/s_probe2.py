# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from mk import *
Q = "?"
write("xj-probe-mute-snooze", ["xj exploration: mute, snooze, hidden tag"], [
 start(), inbox(), sel("XJ Plain note"),
 R("createTag", name="Junk", text="red"), R("createTag", name="Feina", text="blue"),
 R("toggleTag", text="Junk"), idle, R("toggleTag", text="Feina"), idle, E("pause", text="600"),
 {"op": "search", "text": "tag:Feina"}, idle, sel("XJ Plain note"), cap("search-tag-feina"),
 exp({"reading:selectedKeywords": Q, "reading:tagDots:0": Q}),
 {"op": "search", "text": "has:tag"}, idle, exp({"reading:subjects": Q}),
 {"op": "search", "text": ""}, idle,
 inbox(), sel("XJ Roadmap thread"), R("toggleMute"), idle, wf("selectedMuted", "true"), cap("muted-optimistic"),
 inbox(), cap("muted-after-reload"),
 sel("Re: XJ Roadmap thread"), exp({"reading:selectedMuted": Q, "reading:selectedSubject": Q}),
 sel("XJ Roadmap thread"), exp({"reading:selectedMuted": Q}),
 sel("XJ Plain note"), R("remember"), R("snooze", text="3600"), idle, wf("rememberedInList", "false"),
 exp({"reading:snoozedCount": Q, "reading:selectedSubject": Q}), cap("after-snooze"),
 R("selectSnoozed"), idle, cap("snoozed-list"),
 exp({"reading:subjects": Q, "reading:tagDots:0": Q}),
 sel("XJ Plain note"), R("snooze", text="7200"), idle, E("pause", text="800"), cap("resnoozed-in-snoozed-list"),
 exp({"reading:snoozedCount": Q, "reading:messageCount": Q, "reading:errorText": Q}),
 R("selectSnoozed"), idle, exp({"reading:messageCount": Q}),
 sel("XJ Plain note"), R("unsnooze"), idle, E("pause", text="800"), cap("after-unsnooze"),
 exp({"reading:snoozedCount": Q, "reading:messageCount": Q}),
 inbox(), sel("XJ Server keywords"), R("remember"), R("snooze", text="-500"), idle, E("pause", text="1500"), cap("snoozed-in-the-past"),
 exp({"reading:rememberedInList": Q, "reading:rememberedSnoozed": Q, "reading:snoozedCount": Q}),
])
