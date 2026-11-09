# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from mk import *
Q = "?"
def unsub(subject, tag):
    return [sel(subject), cap("open-" + tag), A("click", window="main", text="928,145", name="sheet:" + tag),
            E("pause", text="700"), cap("sheet-" + tag), exp({"reading:errorText": Q}, finds=[{"sheetAttached": True}])]
write("xj-probe-unsub", ["xj exploration: unsubscribe"], [
 start(), inbox(),
 unsub("XJ Shop offers (https only)", "https"), {"op": "dismissSheet", "window": "sheet:https"}, E("pause", text="500"),
 unsub("XJ Club news (mailto only)", "mailto"), {"op": "dismissSheet", "window": "sheet:mailto"}, E("pause", text="500"),
 unsub("XJ Weekly digest (one-click)", "oneclick"), {"op": "dismissSheet", "window": "sheet:oneclick"}, E("pause", text="500"),
 sel("XJ Legacy list (http only)"), cap("open-http-only"),
])
