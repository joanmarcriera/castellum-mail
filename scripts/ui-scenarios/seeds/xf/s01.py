# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
same = E(("reading:messageCount", "6"), ("ext:pendingCount", "0"), ("reading:selectedSubject", "XF a msg 3"), ("reading:hasSubject:XF a msg 3", "true"))
write("xf-readonly-refuses-all", ["archive", "delete", "flag", "read", "evaluation-mode"], [
    add("a"), mbox("a", "inbox"), IDLE,
    E(("reading:messageCount", "6"), ("ext:pendingCount", "0")),
    msg("XF a msg 3"), IDLE, cap("selected"),
    *refused("archive", "archive"), same,
    *refused("delete", "delete"), same,
    *refused("toggleFlag", "flag"), same, E(("reading:cell:flag:2", "")),
    *refused("toggleRead", "read"), same, E(("reading:selectedUnread", "false")),
    *refused("markAllRead", "markall"), same, cap("end"),
    E(("reading:cell:unread:0", "")),
])
