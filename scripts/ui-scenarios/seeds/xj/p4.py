# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer, second pass: one-click in evaluation mode, untag after reload, project detail, server keyword removal."""
from mk import *
import json
A2 = "xj-org2@local.test"
def dump(*keys): return exp({k: "?" for k in keys})
P = lambda ms: E("pause", text=str(ms))
LINK = "928,145"
def link(tag): return [A("click", window="main", text=LINK, name="sheet:" + tag), P(700)]
write("xj-probe-oneclick", ["probe"], [
 start(), inbox(), sel("XJ Weekly digest (one-click)"), link("oc"), {"op": "wait", "until": "sheet:oc"}, cap("one-click-sheet"),
 dump("reading:errorText"),
])
write("xj-probe-untag", ["probe"], [
 start(A2), E("enableWrites", email=A2), inbox(), R("createTag", name="Feina", text="blue"),
 sel("XJ Roadmap thread"), dump("reading:selectedKeywords"), cap("server-tagged-row"),
 R("toggleTag", text="Feina"), idle, P(4000), dump("reading:selectedKeywords", "ext:pendingCount", "reading:errorText"),
 {"op": "search", "text": "tag:Feina"}, idle, dump("reading:subjects"),
])
write("xj-probe-project-detail", ["probe"], [
 start(), inbox(), sel("XJ Roadmap thread"), R("createProjectAssign", name="Roadmap"), idle,
 sel("XJ Plain note"), R("createProjectAssign", name="Roadmap"), idle, dump("reading:projectCount"),
 {"op": "openWindow", "target": "projects"}, {"op": "wait", "until": "window:projects"}, idle, cap("board", ["projects"]),
 A("click", window="projects", text="100,120"), P(1200), cap("after-card-click"),
 E("describe", window="projects"),
])
write("xj-probe-server-kw-eval", ["probe"], [
 start(), inbox(), sel("XJ Server keywords"), {"op": "search", "text": "tag:xjServerTag"}, idle, sel("XJ Server keywords"),
 dump("reading:selectedKeywords"), R("toggleTag", text="xjServerTag"), idle, P(1500),
 dump("reading:selectedKeywords", "reading:tagNotice", "ext:pendingCount"),
 R("reSyncAll"), P(8000), idle, {"op": "search", "text": "tag:xjServerTag"}, idle, dump("reading:subjects"),
])
