# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer, session 2: kept scenarios added on top of all.py. Usage: python3 s2.py (writes scripts/ui-scenarios/xf-*.json, new names only)"""
from gen import *
def lab(step, text): step["label"] = text; return step
def EL(text, *pairs): return lab(E(*pairs), text)
def WS(label, text, t=5000): return lab(W("sheet:" + label, t), text)
def ep(ref, hp): return A("setEndpoint", accountRef=ref, text=hp)
def q(text): return {"op": "search", "text": text}
UNREAD = lambda n, t, *more: [q("is:unread"), IDLE, settle(600), EL(t, ("reading:messageCount", str(n)), *more), q(""), IDLE, settle(400)]

# --- 11. server unreachable: delete + archive queue, nothing resurrects, replay when it is back -------
write("xf-offline-queue-replays", ["delete", "archive", "offline", "replay", "sync-banner"], [
    add("o"), writes("o"), mbox("o", "inbox"), IDLE, EL("5 seeded", ("reading:messageCount", "5")),
    ep("o", "127.0.0.1:3999"), settle(1500),
    msg("XF o msg 4"), IDLE, R("deleteSelected"), settle(1000), cap("deleted-while-unreachable"),
    EL("delete while the server is down: row leaves, no error dialog", ("reading:messageCount", "4"), ("reading:hasSubject:XF o msg 4", "false"), ("reading:errorText", "none")),
    msg("XF o msg 2"), IDLE, cmd("archive"), settle(1000), R("reSyncAll"), settle(6000), cap("failed-sync-while-queued"),
    EL("a failed sync while the moves are queued resurrects nothing", ("reading:messageCount", "3"), ("reading:hasSubject:XF o msg 4", "false"), ("reading:hasSubject:XF o msg 2", "false"), ("reading:errorText", "none")),
    ep("o", "127.0.0.1:3143"), settle(1000), R("reSyncAll"), settle(3000), drained(60000), IDLE, settle(2000),
    mbox("o", "inbox"), IDLE, settle(600), EL("back online: queue drained, Inbox still 3", ("reading:messageCount", "3"), ("reading:errorText", "none"), ("reading:syncBanner", "none")),
    mbox("o", "trash"), IDLE, waitR("hasSubject:XF o msg 4", "true"), EL("the delete reached the server Trash", ("reading:messageCount", "1")), cap("trash-after-replay"),
], timeout=300000)

# --- 12. read / unread / mark all read with writes on ----------------------------------------------
write("xf-read-unread-markall", ["read", "unread", "mark-all-read", "counts"], [
    add("m"), writes("m"), mbox("m", "inbox"), IDLE, *UNREAD(5, "5 unread to start"),
    msg("XF m msg 5"), IDLE, settle(3000), drained(), EL("opening marks it read", ("reading:selectedUnread", "false")),
    cmd("toggleRead"), settle(800), EL("Mark as Unread", ("reading:selectedUnread", "true")),
    settle(3000), EL("it stays unread while it is still open", ("reading:selectedUnread", "true")),
    R("clearMessage"), drained(), R("reSyncAll"), settle(4000), IDLE,
    *UNREAD(5, "unread again after replay + re-sync", ("reading:hasSubject:XF m msg 5", "true")),
    mbox("m", "inbox"), IDLE, cmd("markAllRead"), settle(1000), drained(), cap("after-mark-all"),
    *UNREAD(0, "Mark All as Read: none unread"),
    R("reSyncAll"), settle(4000), IDLE, *UNREAD(0, "still none after re-sync", ("reading:errorText", "none")),
], timeout=300000)

# --- 13. Archive out of Trash; Delete then Empty Trash straight away --------------------------------
write("xf-trash-archive-restore-empty-race", ["trash", "archive", "empty-trash", "race"], [
    add("p"), writes("p"), mbox("p", "inbox"), IDLE,
    msg("XF p msg 1"), R("deleteSelected"), settle(600), drained(), IDLE, settle(1500),
    mbox("p", "trash"), IDLE, waitR("hasSubject:XF p msg 1", "true"), msg("XF p msg 1"), IDLE, cmd("archive"), settle(1200),
    EL("Archive inside Trash moves the message out: no permanent-delete prompt, no error", ("reading:pendingDelete", "false"), ("reading:errorText", "none"), ("reading:messageCount", "0")),
    drained(), IDLE, settle(1500), EL("Trash stays empty after replay", ("reading:messageCount", "0")),
    mbox("p", "inbox"), IDLE, EL("Inbox has 4", ("reading:messageCount", "4")),
    msg("XF p msg 2"), R("deleteSelected"), reg("sheet:empty"), cmd("emptyTrash"), settle(900), WS("empty", "Empty Trash asks first"), capall("empty-trash-right-after-delete"),
    press("sheet:empty", "Empty Trash"), settle(600), drained(60000), IDLE, settle(2500),
    EL("Inbox: only the deleted one is gone", ("reading:messageCount", "3"), ("reading:hasSubject:XF p msg 2", "false"), ("reading:errorText", "none")),
    mbox("p", "trash"), IDLE, settle(1500), EL("the message deleted just before Empty Trash is erased too", ("reading:messageCount", "0")),
    R("reSyncAll"), settle(4000), IDLE, mbox("p", "inbox"), IDLE, settle(600),
    EL("and it does not come back with the next sync", ("reading:messageCount", "3"), ("reading:hasSubject:XF p msg 2", "false"), ("reading:errorText", "none")), cap("end"),
], timeout=300000)

# --- 14. DEFECT: Mark All as Read during a search marks the mail the search hid ---------------------
write("xf-defect-mark-all-read-ignores-search", ["mark-all-read", "search", "read"], [
    add("q"), writes("q"), mbox("q", "inbox"), IDLE, q("XF q msg 2"), IDLE, settle(700),
    EL("the search lists one message", ("reading:messageCount", "1"), ("reading:hasSubject:XF q msg 2", "true")), cap("search-shows-one"),
    cmd("markAllRead"), settle(1000), drained(), q(""), IDLE, settle(500), q("is:unread"), IDLE, settle(700), cap("unread-after-mark-all"),
    EL("Mark All as Read with a search showing 1 message must leave the 4 messages the search hid unread", ("reading:messageCount", "4")),
], timeout=300000)

# --- 15. DEFECT: Mark All as Read in All Inboxes silently does nothing -------------------------------
write("xf-defect-mark-all-read-unified-noop", ["mark-all-read", "unified", "read"], [
    add("r"), writes("r"), R("selectUnified", text="inbox"), IDLE, EL("All Inboxes, 5 unread listed", ("reading:isUnified", "true"), ("reading:messageCount", "5")),
    cmd("markAllRead"), settle(1500), drained(), cap("after-mark-all-in-all-inboxes"),
    q("is:unread"), IDLE, settle(700),
    EL("Mark All as Read in All Inboxes must mark the listed mail read (it is offered, has a shortcut, and says nothing when it does nothing)", ("reading:messageCount", "0")),
], timeout=300000)
