# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from mk import *
Q = "?"
B = lambda l: {"role": "button", "label": l}
write("xj-probe-unsub-mailto", ["xj exploration: mailto unsubscribe in evaluation mode"], [
 start(), inbox(), sel("XJ Club news (mailto only)"),
 A("click", window="main", text="928,145", name="sheet:unsub"), {"op": "wait", "until": "sheet:unsub"},
 A("click", window="sheet:unsub", text="310,117"), E("pause", text="2000"),
 cap("after-confirm"),
 exp({"reading:errorText": Q, "ext:pendingCount": Q}, finds=[{"sheetAttached": False}]),
 E("openFirstDraft", email=ACCT), E("pause", text="800"), cap("orphan-draft"),
 exp({"ext:composeWindows": Q, "ext:subject": Q}),
])
