# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer: probe mailto unsubscribe with writes enabled."""
from mk import *
A3 = "xj-org3@local.test"
def dump(*keys): return exp({k: "?" for k in keys})
P = lambda ms: E("pause", text=str(ms))
write("xj-probe-mailto-send", ["probe"], [
 start(A3), E("enableWrites", email=A3), inbox(), sel("XJ Listserv (mailto body)"),
 A("click", window="main", text="928,145", name="sheet:m"), P(700), {"op": "wait", "until": "sheet:m"}, cap("sheet"),
 A("click", window="sheet:m", text="310,117"), P(6000), cap("after-send"),
 dump("reading:errorText", "ext:pendingCount", "ext:undoBanner", "ext:drafts:" + A3),
 {"op": "addAccount", "email": "xj-signoff3@local.test", "password": "xj-signoff3@local.test", "as": "so", "waitFor": "synced", "timeoutMs": 60000},
 inbox("so"), dump("reading:subjects"), sel("unsubscribe"), P(1500), cap("received"), dump("reading:detailTextHas:SIGNOFF", "reading:selectedSubject"),
])
