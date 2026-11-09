# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer, second pass: mailto unsubscribe says "sent" but the message stays queued; body= probe."""
from mk import *
A3 = "xj-org3@local.test"
P = lambda ms: E("pause", text=str(ms))
def dump(*keys): return exp({k: "?" for k in keys})
write("xj-defect-unsubscribe-mailto-says-sent-but-stays-queued", ["S44 List-Unsubscribe mailto", "writes enabled", "confirmUnsubscribeByMailto queues the send and never drains the outbox", "xj defect"], [
 start(A3), E("enableWrites", email=A3), inbox(), sel("XJ Club news (mailto subject)"),
 A("click", window="main", text="928,145", name="sheet:m"), P(700), {"op": "wait", "until": "sheet:m"}, cap("confirm-sheet"),
 A("click", window="sheet:m", text="310,117"), P(4000), cap("app-says-sent"),
 P(40000),
 {"op": "openWindow", "target": "outbox"}, {"op": "wait", "until": "window:outbox"}, idle, P(1500), cap("outbox-45s-later", ["outbox"]),
 exp({"ext:pendingCount": "0", "reading:errorText": "none"}),
], timeout=200000)
write("xj-probe-mailto-body", ["probe"], [
 start(A3), inbox(), sel("XJ Listserv (mailto body)"),
 A("click", window="main", text="928,145", name="sheet:m"), P(700), {"op": "wait", "until": "sheet:m"}, cap("confirm-sheet"),
 A("click", window="sheet:m", text="310,117"), P(2000),
 E("openFirstDraft", email=A3, timeoutMs=4000), P(800), cap("draft"), dump("ext:body", "ext:subject", "ext:bodyHas:SIGNOFF"),
])
