# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer, second pass: one-click unsubscribe while the account is in evaluation mode; local-only tag removal."""
from mk import *
def dump(*keys): return exp({k: "?" for k in keys})
P = lambda ms: E("pause", text=str(ms))
write("xj-probe-oneclick-eval", ["probe"], [
 start(), inbox(), sel("XJ Weekly digest (one-click)"), A("click", window="main", text="928,145", name="sheet:oc"), P(700),
 {"op": "wait", "until": "sheet:oc"}, A("click", window="sheet:oc", text="310,141"), P(4000), cap("after-confirm"),
 dump("reading:errorText", "ext:pendingCount"),
])
write("xj-probe-untag-local", ["probe"], [
 start(), inbox(), {"op": "search", "text": "tag:xjServerTag"}, idle, sel("XJ Server keywords"),
 R("toggleTag", text="xjServerTag"), idle, P(1500), cap("after-local-removal"), dump("reading:selectedKeywords", "reading:tagNotice"),
 {"op": "search", "text": ""}, idle, {"op": "search", "text": "tag:xjServerTag"}, idle, dump("reading:subjects"),
])
