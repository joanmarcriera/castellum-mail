# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer: probe - is the mailto unsubscribe email ever delivered?"""
from mk import *
A3 = "xj-org3@local.test"
P = lambda ms: E("pause", text=str(ms))
write("xj-probe-mailto-outbox", ["probe"], [
 start(A3), E("enableWrites", email=A3), inbox(), sel("XJ Listserv (mailto body)"),
 A("click", window="main", text="928,145", name="sheet:m"), P(700), {"op": "wait", "until": "sheet:m"},
 A("click", window="sheet:m", text="310,117"), P(3000),
 {"op": "openWindow", "target": "outbox"}, {"op": "wait", "until": "window:outbox"}, idle, P(1000), cap("outbox-3s", ["outbox"]),
 P(75000), idle, cap("outbox-80s", ["outbox"]),
], timeout=240000)
