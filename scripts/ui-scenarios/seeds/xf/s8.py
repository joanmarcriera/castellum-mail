# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf UIT-1 snippet scenarios. Usage: python3 scripts/ui-scenarios/seeds/xf/seed_sort.py && python3 scripts/ui-scenarios/seeds/xf/s8.py
(the sort seed account xf-s@local.test is reused: the scenario only needs a synced account).
Writes xf-snippets-insert.json: add a snippet, open a compose window, insert it through the body's NSTextView
(what choosing it in the picker does; the sheet itself cannot be clicked headlessly, #3158), assert the filled
body, open the picker through the command and the Snippets window."""
import os, gen
gen.ROOT = os.path.abspath(os.path.dirname(os.path.abspath(__file__)) + "/../../../..")
from gen import *
def lab(step, text): step["label"] = text; return step
def EL(text, *pairs): return lab(E(*pairs), text)
ADD = {"op": "addAccount", "email": "xf-s@local.test", "password": "xf-s@local.test", "as": "s", "waitFor": "synced", "timeoutMs": 90000}
write("xf-snippets-insert", ["snippets", "compose", "placeholders"], [
    ADD, IDLE,
    X("snippetAdd", name="Greeting", text="Hi ${recipient.first|there},${cursor} bye", tab="hi"),
    EL("the snippet is stored", ("ext:snippetCount", "1")),
    {"op": "openWindow", "target": "compose"}, {"op": "wait", "until": "window:compose"}, IDLE,
    X("setField", window="compose", name="to", text="Grace Hopper <grace@x.test>"), IDLE,
    X("insertSnippet", text="Greeting"), IDLE, settle(300),
    EL("placeholders are filled from the first To address", ("ext:body", "Hi Grace, bye")),
    {"op": "capture", "name": "compose-after-insert", "windows": ["compose"]},
    X("openSnippetPicker"), IDLE, settle(500),
    EL("the Insert Snippet command opens the picker on the compose window", ("ext:snippetPicker", "shown")),
    {"op": "capture", "name": "picker", "windows": ["all"]},
    X("showSnippets"), {"op": "wait", "until": "window:title:Snippets", "timeoutMs": 10000}, IDLE, settle(500),
    {"op": "capture", "name": "snippets-window", "windows": ["all"]},
    EL("the Snippets window is open", ("ext:snippetsWindow", "open")),
], timeout=240000)
