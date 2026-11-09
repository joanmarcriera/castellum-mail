# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from mk import *
write("xj-probe-tags", ["xj exploration: tags"], [
 start(), inbox(), sel("XJ Server keywords"), cap("server-keywords-message"),
 exp({"reading:selectedKeywords": "?", "reading:tagNames": "?", "reading:messageCount": "?"}),
 R("createTag", name="Junk", text="red"), R("createTag", name="Feina urgent", text="blue"),
 R("createTag", name="A very long tag name that a person might type when they treat tags like notes to themselves about what to do next week", text="green"),
 sel("XJ Plain note"), R("toggleTag", text="Junk"), idle, R("toggleTag", text="Feina_urgent"), idle,
 R("toggleTag", text="A_very_long_tag_name_that_a_person_might_type_when_they_treat_tags_like_notes_to_themselves_about_what_to_do_next_week"), idle,
 E("pause", text="800"), cap("tagged-three"),
 exp({"reading:selectedKeywords": "?", "reading:tagNotice": "?", "reading:tagNames": "?"}),
 inbox(), sel("XJ Plain note"), cap("after-reload"),
 exp({"reading:selectedKeywords": "?"}),
 {"op": "search", "text": "tag:Junk"}, idle, exp({"reading:subjects": "?"}), cap("search-tag-junk"),
 {"op": "search", "text": ""}, idle,
 {"op": "wait", "until": "settingsOpenerReady"}, R("settingsTab", tab="tags"), {"op": "wait", "until": "window:settings"}, idle,
 cap("settings-tags", ["settings"]),
])
