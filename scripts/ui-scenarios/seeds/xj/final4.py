# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer, second pass: mailto unsubscribe says "sent" but the message stays queued (kept version)."""
from mk import *
A3 = "xj-org3@local.test"
P = lambda ms: E("pause", text=str(ms))
write("xj-defect-unsubscribe-mailto-says-sent-but-stays-queued", ["S44 List-Unsubscribe mailto", "writes enabled", "confirmUnsubscribeByMailto queues the send and never drains the outbox", "xj defect"], [
 start(A3), E("enableWrites", email=A3), inbox(), sel("XJ Club news (mailto subject)"),
 A("click", window="main", text="928,145", name="sheet:m"), P(700), {"op": "wait", "until": "sheet:m"}, cap("confirm-sheet"),
 A("click", window="sheet:m", text="310,117"), P(4000), cap("app-says-sent"),
 A("click", window="class:_NSAlertPanel", text="130,90"), P(40000),
 {"op": "openWindow", "target": "outbox"}, {"op": "wait", "until": "window:outbox"}, idle, P(1500), cap("outbox-45s-later", ["outbox"]),
 {"op": "assert", "window": "outbox", "expect": [
   {"find": {"role": "staticText", "label": "xj-org3@local.test"}, "exists": True},
   {"find": {"role": "staticText", "label": "Queued"}, "exists": False}]},
], timeout=200000)
