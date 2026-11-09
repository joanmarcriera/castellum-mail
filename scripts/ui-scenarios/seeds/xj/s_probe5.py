# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from mk import *
import json
Q = "?"
def smart(name, value): return R("upsertSmart", name=name, accountRef="xj", text=json.dumps({"op": "and", "conditions": [{"field": "tag", "operator": "is", "value": value}], "groups": []}))
LONG = "Projecte amb un nom molt llarg per veure com es comporta el tauler quan algu escriu una frase sencera com a titol"
write("xj-probe-smart-projects", ["xj exploration: tag smart mailboxes, correspondence, projects, one-click sheet"], [
 start(), inbox(),
 R("createTag", name="Feina urgent", text="blue"), R("createTag", name="Junk", text="red"),
 sel("XJ Plain note"), R("toggleTag", text="Feina_urgent"), idle,
 sel("XJ Club news (mailto only)"), R("toggleTag", text="Junk"), idle, E("pause", text="600"),
 smart("Any tag", True), smart("No tag", False), smart("Feina by name", "Feina urgent"), smart("Junk by name", "Junk"), smart("Server tag", "xjServerTag"),
 R("selectSmart", text="Any tag"), idle, exp({"reading:subjects": Q, "reading:tagDots:0": Q}), cap("smart-any-tag"),
 R("selectSmart", text="No tag"), idle, exp({"reading:messageCount": Q}),
 R("selectSmart", text="Feina by name"), idle, exp({"reading:subjects": Q}),
 R("selectSmart", text="Junk by name"), idle, exp({"reading:subjects": Q}),
 R("selectSmart", text="Server tag"), idle, exp({"reading:subjects": Q}),
 inbox(), sel("XJ Roadmap thread"), R("correspondence", text="pau@partner.example.test"), idle, cap("correspondence-pau"),
 exp({"reading:corrTotal": Q, "reading:corrSent": Q, "reading:corrReceived": Q, "reading:messageCount": Q, "reading:corrMonths": Q}),
 R("correspondence", text="nobody at all"), idle, cap("correspondence-garbage"),
 exp({"reading:correspondence": Q, "reading:corrTotal": Q, "reading:messageCount": Q}),
 R("closeCorrespondence"), idle, exp({"reading:mailboxRole": Q, "reading:messageCount": Q}),
 inbox(), sel("XJ Plain note"), R("createProjectAssign", name=LONG), idle,
 R("createProjectAssign", name=LONG), idle, exp({"reading:projectCount": Q}),
 R("projectLinks"), exp({"reading:note": Q}),
 {"op": "openWindow", "target": "projects"}, {"op": "wait", "until": "window:projects"}, idle, cap("projects-board", ["projects"]),
 {"op": "resizeWindow", "window": "projects", "contentSize": "min"}, idle, cap("projects-board-min", ["projects"]),
 {"op": "assert", "window": "projects", "rules": ["nonBlank", "fitsScreen", "noClippedText", "controlsReachable"]},
 {"op": "closeWindow", "window": "projects"},
 sel("XJ Weekly digest (one-click)"), A("click", window="main", text="928,145", name="sheet:unsub"), {"op": "wait", "until": "sheet:unsub"}, E("pause", text="500"),
 cap("one-click-sheet"),
])
