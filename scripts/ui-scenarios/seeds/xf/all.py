# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer: every kept scenario. Usage: python3 all.py  (rewrites scripts/ui-scenarios/xf-*.json)"""
from gen import *
def lab(step, text): step["label"] = text; return step
def S(v, text): return lab(sheet(v), text)
def EL(text, *pairs): return lab(E(*pairs), text)

# --- 1. DEFECT: a refused write shows nothing at all --------------------------------------------
write("xf-defect-readonly-refusal-silent", ["evaluation-mode", "archive", "delete", "flag", "WS1"], [
    add("a"), mbox("a", "inbox"), IDLE, msg("XF a msg 3"), IDLE,
    reg("sheet:blocked-archive"), cmd("archive"), IDLE, settle(1200), capall("after-archive"),
    S(True, "Archive on a read-only account must show the 'This Account Is Read-only' dialog"),
    R("deleteSelected"), IDLE, settle(1200), capall("after-delete"),
    S(True, "Delete on a read-only account must show the read-only dialog"),
    cmd("toggleFlag"), IDLE, settle(1200), capall("after-flag"),
    S(True, "Flag on a read-only account must show the read-only dialog"),
    EL("and nothing was applied", ("reading:messageCount", "6"), ("ext:pendingCount", "0")),
])

# --- 2. read-only never half-applies (single mailbox + unified, two accounts) ---------------------
same_a = lambda t: EL(t, ("reading:messageCount", "6"), ("ext:pendingCount", "0"), ("reading:hasSubject:XF a msg 3", "true"), ("reading:errorText", "none"))
same_u = lambda t: EL(t, ("reading:messageCount", "11"), ("ext:pendingCount", "0"), ("reading:hasSubject:XF b msg 2", "true"), ("reading:errorText", "none"))
write("xf-readonly-never-half-applies", ["evaluation-mode", "archive", "delete", "flag", "read", "unified"], [
    add("a"), add("b"), mbox("a", "inbox"), IDLE, msg("XF a msg 3"), IDLE, same_a("baseline"),
    cmd("archive"), IDLE, settle(900), same_a("archive refused, row kept, nothing queued"),
    R("deleteSelected"), IDLE, settle(900), same_a("delete refused"),
    cmd("toggleFlag"), IDLE, settle(900), same_a("flag refused"),
    cmd("toggleRead"), IDLE, settle(900), same_a("mark unread refused"), EL("unread flip reverted", ("reading:selectedUnread", "false")),
    cmd("markAllRead"), IDLE, settle(900), same_a("mark all read refused"), cap("single-after-all"),
    R("clearMessage"), cmd("archive"), R("deleteSelected"), IDLE, settle(500), same_a("no selection: no-op"),
    R("selectUnified", text="inbox"), IDLE, EL("unified", ("reading:isUnified", "true"), ("reading:messageCount", "11")),
    msg("XF b msg 2"), IDLE, cmd("archive"), IDLE, settle(900), same_u("unified archive refused"),
    R("deleteSelected"), IDLE, settle(900), same_u("unified delete refused"),
    cmd("toggleFlag"), IDLE, settle(900), same_u("unified flag refused"), cap("unified-after-all"),
])

# --- 3. single archive / delete / read with writes on --------------------------------------------
write("xf-archive-delete-single", ["archive", "delete", "read", "counts"], [
    add("c"), writes("c"), mbox("c", "inbox"), IDLE, EL("5 seeded", ("reading:messageCount", "5")),
    msg("XF c msg 3"), IDLE, cmd("toggleFlag"), IDLE, settle(500), cap("flagged"),
    cmd("archive"), settle(500), EL("archived row leaves at once", ("reading:messageCount", "4"), ("reading:hasSubject:XF c msg 3", "false"), ("reading:errorText", "none")),
    cap("after-archive"), drained(), IDLE, settle(1500),
    EL("still gone after replay + re-sync", ("reading:messageCount", "4"), ("reading:hasSubject:XF c msg 3", "false")),
    msg("XF c msg 1"), R("deleteSelected"), settle(500), EL("deleted row leaves", ("reading:messageCount", "3"), ("reading:hasSubject:XF c msg 1", "false")),
    drained(), IDLE, settle(1500), EL("still gone", ("reading:messageCount", "3")),
    msg("XF c msg 5"), IDLE, cmd("toggleRead"), settle(500), EL("marked unread", ("reading:selectedUnread", "true")),
    cmd("toggleRead"), settle(500), EL("and read again", ("reading:selectedUnread", "false")), drained(),
    mbox("c", "trash"), IDLE, settle(800), EL("Trash holds exactly the deleted one", ("reading:messageCount", "1"), ("reading:hasSubject:XF c msg 1", "true")), cap("trash"),
    mbox("c", "archive"), IDLE, settle(800), cap("archive"),
    dump("reading:messageCount", "reading:subjects", "reading:cell:unread:0", "reading:cell:flag:0", win=False),
], timeout=300000)

# --- 4. Trash: permanent delete with confirmation, cancel, twice, read-only, empty trash ---------
write("xf-trash-permanent-delete", ["trash", "permanent-delete", "confirmation", "empty-trash", "min-window"], [
    add("d"), writes("d"), mbox("d", "inbox"), IDLE,
    msg("XF d msg 2"), R("deleteSelected"), settle(400), msg("XF d msg 4"), R("deleteSelected"), settle(400), drained(), IDLE, settle(1500),
    mbox("d", "trash"), IDLE, settle(800), EL("two in Trash", ("reading:messageCount", "2")),
    msg("XF d msg 2"), IDLE, reg("sheet:confirm1"), R("deleteSelected"), settle(800),
    lab({"op": "assert", "expect": [{"sheetAttached": True}, {"windowCount": 2}, {"model": "reading:pendingDelete", "equals": "true"}, {"model": "ext:pendingCount", "equals": "0"}]}, "Delete in Trash asks first, queues nothing"),
    capall("confirm-dialog"),
    R("deleteSelected"), settle(500), lab({"op": "assert", "expect": [{"windowCount": 2}]}, "Delete twice = still one dialog"),
    press("sheet:confirm1", "Cancel"), settle(800),
    lab({"op": "assert", "expect": [{"sheetAttached": False}, {"model": "reading:pendingDelete", "equals": "false"}, {"model": "reading:messageCount", "equals": "2"}, {"model": "ext:pendingCount", "equals": "0"}]}, "Cancel deletes nothing"),
    reg("sheet:confirm2"), R("deleteSelected"), settle(800), press("sheet:confirm2", "Delete Permanently"), settle(500),
    drained(), waitR("hasSubject:XF d msg 2", "false", 30000), IDLE,
    EL("confirmed: gone, the other stays", ("reading:messageCount", "1"), ("reading:hasSubject:XF d msg 4", "true"), ("reading:errorText", "none")), cap("after-permanent"),
    R("setWrite", accountRef="d", value=False), msg("XF d msg 4"), IDLE, reg("sheet:confirm3"), R("deleteSelected"), settle(800),
    press("sheet:confirm3", "Delete Permanently"), settle(1500),
    EL("read-only again: confirmed permanent delete is refused, row stays, nothing queued", ("reading:messageCount", "1"), ("reading:hasSubject:XF d msg 4", "true"), ("ext:pendingCount", "0"), ("reading:errorText", "none")),
    R("setWrite", accountRef="d", value=True), {"op": "resizeWindow", "window": "main", "contentSize": "min"}, IDLE,
    reg("sheet:empty", at="300,150"), cmd("emptyTrash"), settle(900), capall("empty-trash-dialog-min-window"),
    lab({"op": "assert", "expect": [{"sheetAttached": True}, {"model": "reading:messageCount", "equals": "1"}]}, "Empty Trash asks first"),
    press("sheet:empty", "Empty Trash"), settle(600), drained(), waitR("messageCount", "0", 30000), IDLE, cap("trash-empty"),
    mbox("d", "inbox"), IDLE, EL("Inbox untouched by Empty Trash", ("reading:messageCount", "3")),
], timeout=300000)

# --- 5. unified inbox across two accounts (one writable, one read-only) ---------------------------
write("xf-unified-two-accounts", ["unified", "archive", "delete", "evaluation-mode", "counts"], [
    add("e"), add("f"), writes("e"), R("selectUnified", text="inbox"), IDLE, EL("10 across two accounts", ("reading:messageCount", "10"), ("reading:isUnified", "true")),
    msg("XF e msg 2"), IDLE, R("deleteSelected"), settle(500), EL("writable account: deleted", ("reading:messageCount", "9"), ("reading:hasSubject:XF e msg 2", "false")),
    msg("XF f msg 2"), IDLE, R("deleteSelected"), settle(1200), EL("read-only account: refused, row kept", ("reading:messageCount", "9"), ("reading:hasSubject:XF f msg 2", "true"), ("reading:errorText", "none")),
    msg("XF e msg 4"), IDLE, cmd("archive"), settle(500), EL("archived", ("reading:messageCount", "8")),
    msg("XF f msg 4"), IDLE, cmd("archive"), settle(1200), EL("read-only archive refused", ("reading:messageCount", "8"), ("reading:hasSubject:XF f msg 4", "true")),
    drained(), IDLE, settle(1500), EL("stable after replay", ("reading:messageCount", "8"), ("reading:isUnified", "true")), cap("unified-after"),
    mbox("e", "inbox"), IDLE, EL("e inbox", ("reading:messageCount", "3")),
    mbox("f", "inbox"), IDLE, EL("f inbox untouched", ("reading:messageCount", "5")),
    mbox("e", "trash"), IDLE, settle(800), EL("e trash", ("reading:messageCount", "1"), ("reading:hasSubject:XF e msg 2", "true")),
], timeout=300000)

# --- 6. DEFECT: Delete on a Trash message found by search = dead-end error -----------------------
write("xf-defect-delete-trashed-from-search", ["delete", "trash", "search"], [
    add("g"), writes("g"), mbox("g", "inbox"), IDLE, msg("XF g msg 1"), R("deleteSelected"), settle(400), drained(), IDLE, settle(1500),
    R("searchScope", text="account"), {"op": "search", "text": "XF g"}, IDLE, settle(900),
    EL("the search (account scope) lists the trashed message next to inbox mail", ("reading:hasSubject:XF g msg 1", "true"), ("reading:mailboxRole", "inbox")), cap("search-results"),
    msg("XF g msg 1"), IDLE, R("deleteSelected"), settle(1500), capall("after-delete"),
    EL("Delete on a message already in Trash must ask for the permanent delete (or do it), never fail with a dead-end error",
       ("reading:errorText", "none"), ("reading:pendingDelete", "true")),
], timeout=300000)

# --- 7. DEFECT: an Inbox message found by search while Trash is selected gets the permanent prompt
write("xf-defect-permanent-prompt-for-inbox-message", ["delete", "trash", "search", "permanent-delete"], [
    add("i"), writes("i"), mbox("i", "inbox"), IDLE, msg("XF i msg 1"), R("deleteSelected"), settle(400), drained(), IDLE, settle(1500),
    mbox("i", "trash"), IDLE, settle(800), R("searchScope", text="account"), {"op": "search", "text": "XF i"}, IDLE, settle(900),
    EL("Trash selected, account-scope search lists Inbox mail", ("reading:hasSubject:XF i msg 5", "true"), ("reading:mailboxRole", "trash")),
    msg("XF i msg 5"), IDLE, reg("sheet:wrong"), R("deleteSelected"), settle(1000), capall("prompt-for-inbox-message"),
    EL("an Inbox message is moved to Trash: no 'Permanently delete' prompt", ("reading:pendingDelete", "false")),
    press("sheet:wrong", "Delete Permanently"), settle(1500), capall("after-confirm"),
    EL("and confirming must not end in an error", ("reading:errorText", "none")),
], timeout=300000)

# --- 8. awkward ways: twice, nothing selected, long subject, min window, switch mid-action -------
write("xf-awkward-twice-switch-min", ["archive", "delete", "twice", "min-window", "long-subject", "switch-mid-action"], [
    add("h"), writes("h"), mbox("h", "inbox"), IDLE, EL("6 seeded", ("reading:messageCount", "6")),
    R("clearMessage"), cmd("archive"), R("deleteSelected"), cmd("toggleFlag"), cmd("toggleRead"), settle(600),
    EL("nothing selected: nothing happens", ("reading:messageCount", "6"), ("ext:pendingCount", "0"), ("reading:errorText", "none")),
    msg("XF h msg 5"), cmd("archive"), cmd("archive"), cmd("archive"), settle(600),
    EL("Archive three times fast archives one message", ("reading:messageCount", "5"), ("reading:errorText", "none")),
    msg("XF h msg 4"), R("deleteSelected"), R("selectUnified", text="inbox"), IDLE, mbox("h", "inbox"), IDLE, settle(600),
    EL("switching lists mid-delete does not resurrect the row", ("reading:messageCount", "4"), ("reading:hasSubject:XF h msg 4", "false"), ("reading:errorText", "none")),
    {"op": "resizeWindow", "window": "main", "contentSize": "min"}, IDLE, msg("XF h long"), IDLE, cap("min-window-long-subject"),
    cmd("toggleFlag"), settle(300), cmd("toggleFlag"), settle(300), R("deleteSelected"), settle(600),
    EL("long-subject mail deleted at the smallest window", ("reading:messageCount", "3"), ("reading:errorText", "none")),
    drained(), IDLE, settle(1500), EL("stable after replay", ("reading:messageCount", "3"), ("ext:pendingCount", "0")), cap("end"),
    mbox("h", "archive"), IDLE, settle(800), msg("XF h msg 5"), IDLE, cmd("archive"), settle(1500), drained(), IDLE, settle(1500), cap("archive-inside-archive"),
    dump("reading:messageCount", "reading:subjects", "reading:errorText", win=False),
], timeout=300000)

# ======================= revision 2 (overrides the same-named files above) =======================
# An assert while ANY alert/confirmation panel is up trips the harness's implicit noErrorAlert, so dialogs are
# observed with `wait sheet:<label>` / reading waitFor and asserted only after they are dismissed.
def WS(label, text, t=5000): return lab(W("sheet:" + label, t), text)

write("xf-defect-readonly-refusal-silent", ["evaluation-mode", "archive", "WS1"], [
    add("a"), mbox("a", "inbox"), IDLE, msg("XF a msg 3"), IDLE,
    reg("sheet:blocked"), cmd("archive"), IDLE, settle(800), capall("after-archive"),
    WS("blocked", "Archive on a read-only account must present the 'This Account Is Read-only' dialog (Enable Writes / Keep Read-only)"),
    capall("refusal-dialog"), press("sheet:blocked", "Keep Read-only"), settle(600),
    lab({"op": "assert", "expect": [{"sheetAttached": False}, {"model": "reading:messageCount", "equals": "6"}, {"model": "ext:pendingCount", "equals": "0"}]}, "kept read-only: nothing applied"),
])

write("xf-archive-delete-single", ["archive", "delete", "read", "flag", "counts"], [
    add("c"), writes("c"), mbox("c", "inbox"), IDLE, EL("5 seeded", ("reading:messageCount", "5")),
    msg("XF c msg 3"), IDLE, cmd("toggleFlag"), IDLE, settle(500), cap("flagged"),
    {"op": "search", "text": "is:flagged"}, IDLE, settle(500), EL("flag shows in is:flagged", ("reading:messageCount", "1"), ("reading:hasSubject:XF c msg 3", "true")),
    {"op": "search", "text": ""}, IDLE, settle(500), msg("XF c msg 3"), cmd("toggleFlag"), settle(500),
    {"op": "search", "text": "is:flagged"}, IDLE, settle(500), EL("unflag", ("reading:messageCount", "0")), {"op": "search", "text": ""}, IDLE, settle(2500), drained(),
    msg("XF c msg 3"), IDLE, settle(2500), drained(),
    cmd("archive"), settle(500), EL("archived row leaves at once", ("reading:messageCount", "4"), ("reading:hasSubject:XF c msg 3", "false"), ("reading:errorText", "none")),
    cap("after-archive"), drained(), IDLE, settle(1500),
    EL("still gone after replay + re-sync", ("reading:messageCount", "4"), ("reading:hasSubject:XF c msg 3", "false")),
    msg("XF c msg 1"), IDLE, settle(2500), drained(), R("deleteSelected"), settle(500), EL("deleted row leaves", ("reading:messageCount", "3"), ("reading:hasSubject:XF c msg 1", "false")),
    drained(), IDLE, settle(1500), EL("still gone", ("reading:messageCount", "3")),
    msg("XF c msg 5"), IDLE, cmd("toggleRead"), settle(500), EL("marked unread", ("reading:selectedUnread", "true")),
    cmd("toggleRead"), settle(500), EL("and read again", ("reading:selectedUnread", "false")), drained(),
    mbox("c", "trash"), IDLE, waitR("hasSubject:XF c msg 1", "true"), EL("Trash holds exactly the deleted one", ("reading:messageCount", "1")), cap("trash"),
    {"op": "search", "text": "is:unread"}, IDLE, settle(500), EL("it was read before the delete (and the read had synced): still read in Trash", ("reading:hasSubject:XF c msg 1", "false")),
], timeout=300000)

write("xf-trash-permanent-delete", ["trash", "permanent-delete", "confirmation", "empty-trash", "min-window"], [
    add("d"), writes("d"), mbox("d", "inbox"), IDLE,
    msg("XF d msg 2"), R("deleteSelected"), settle(400), msg("XF d msg 4"), R("deleteSelected"), settle(400), drained(), IDLE, settle(1500),
    mbox("d", "trash"), IDLE, waitR("messageCount", "2"), EL("two in Trash", ("reading:messageCount", "2")),
    msg("XF d msg 2"), IDLE, reg("sheet:confirm1"), R("deleteSelected"), settle(800),
    WS("confirm1", "Delete in Trash asks first"), waitR("pendingDelete", "true", 3000), X("waitExt", name="pendingCount", text="0", timeoutMs=2000),
    capall("confirm-dialog"), R("deleteSelected"), settle(500),
    press("sheet:confirm1", "Cancel"), settle(900),
    lab({"op": "assert", "expect": [{"sheetAttached": False}, {"windowCount": 1}, {"model": "reading:pendingDelete", "equals": "false"}, {"model": "reading:messageCount", "equals": "2"}, {"model": "ext:pendingCount", "equals": "0"}]}, "Delete pressed twice = one dialog; Cancel deletes nothing"),
    reg("sheet:confirm2"), R("deleteSelected"), settle(800), press("sheet:confirm2", "Delete Permanently"), settle(500),
    drained(), waitR("hasSubject:XF d msg 2", "false", 30000), IDLE,
    EL("confirmed: gone, the other stays", ("reading:messageCount", "1"), ("reading:hasSubject:XF d msg 4", "true"), ("reading:errorText", "none")), cap("after-permanent"),
    R("setWrite", accountRef="d", value=False), msg("XF d msg 4"), IDLE, reg("sheet:confirm3"), R("deleteSelected"), settle(800),
    press("sheet:confirm3", "Delete Permanently"), settle(1500),
    EL("read-only again: confirmed permanent delete is refused, row stays, nothing queued", ("reading:messageCount", "1"), ("reading:hasSubject:XF d msg 4", "true"), ("ext:pendingCount", "0"), ("reading:errorText", "none")),
    R("setWrite", accountRef="d", value=True), {"op": "resizeWindow", "window": "main", "contentSize": "min"}, IDLE,
    reg("sheet:empty", at="300,150"), cmd("emptyTrash"), settle(900), WS("empty", "Empty Trash asks first"), capall("empty-trash-dialog-min-window"),
    waitR("messageCount", "1", 2000), press("sheet:empty", "Empty Trash"), settle(600), drained(), waitR("messageCount", "0", 30000), IDLE, cap("trash-empty"),
    mbox("d", "inbox"), IDLE, EL("Inbox untouched by Empty Trash", ("reading:messageCount", "3"), ("reading:errorText", "none")),
], timeout=300000)

write("xf-defect-permanent-prompt-for-inbox-message", ["delete", "trash", "search", "permanent-delete"], [
    add("i"), writes("i"), mbox("i", "inbox"), IDLE, msg("XF i msg 1"), R("deleteSelected"), settle(400), drained(), IDLE, settle(1500),
    mbox("i", "trash"), IDLE, settle(800), R("searchScope", text="account"), {"op": "search", "text": "XF i"}, IDLE, settle(900),
    EL("Trash selected, account-scope search lists Inbox mail", ("reading:hasSubject:XF i msg 5", "true"), ("reading:mailboxRole", "trash")),
    msg("XF i msg 5"), IDLE, R("deleteSelected"), settle(1000), capall("prompt-for-inbox-message"),
    EL("an Inbox message is moved to Trash: no 'Permanently delete this message?' prompt (confirming it only ends in an error)", ("reading:pendingDelete", "false"), ("reading:hasSubject:XF i msg 5", "false")),
], timeout=300000)

write("xf-awkward-twice-switch-min", ["archive", "delete", "twice", "min-window", "long-subject", "switch-mid-action"], [
    add("h"), writes("h"), mbox("h", "inbox"), IDLE, EL("6 seeded", ("reading:messageCount", "6")),
    R("clearMessage"), cmd("archive"), R("deleteSelected"), cmd("toggleFlag"), cmd("toggleRead"), settle(600),
    EL("nothing selected: nothing happens", ("reading:messageCount", "6"), ("ext:pendingCount", "0"), ("reading:errorText", "none")),
    msg("XF h msg 5"), cmd("archive"), cmd("archive"), cmd("archive"), settle(600),
    EL("Archive three times fast archives one message", ("reading:messageCount", "5"), ("reading:errorText", "none")),
    msg("XF h msg 4"), R("deleteSelected"), R("selectUnified", text="inbox"), IDLE, mbox("h", "inbox"), IDLE, settle(600),
    EL("switching lists mid-delete does not resurrect the row", ("reading:messageCount", "4"), ("reading:hasSubject:XF h msg 4", "false"), ("reading:errorText", "none")),
    {"op": "resizeWindow", "window": "main", "contentSize": "min"}, IDLE, msg("XF h long"), IDLE, cap("min-window-long-subject"),
    lab({"op": "assert", "window": "main", "rules": ["nonBlank"]}, "smallest window still draws"),
    cmd("toggleFlag"), settle(300), cmd("toggleFlag"), settle(300), R("deleteSelected"), settle(600),
    EL("long-subject mail deleted at the smallest window", ("reading:messageCount", "3"), ("reading:errorText", "none")),
    drained(), IDLE, settle(1500), EL("stable after replay", ("reading:messageCount", "3"), ("ext:pendingCount", "0")), cap("end"),
    R("selectUnified", text="inbox"), IDLE, msg("XF h msg 3"), R("deleteSelected"), mbox("h", "trash"), IDLE, drained(), waitR("hasSubject:XF h msg 3", "true"),
    EL("Trash after the journey", ("reading:messageCount", "3"), ("reading:errorText", "none")),
], timeout=300000)

# --- 9. DEFECT: read / flag set just before archive or delete never reaches the server -----------
write("xf-defect-read-flag-lost-on-move", ["delete", "archive", "flag", "read", "replay"], [
    add("j"), writes("j"), mbox("j", "inbox"), IDLE,
    msg("XF j msg 3"), cmd("toggleFlag"), R("deleteSelected"), settle(300), cap("opened-flagged-deleted"),
    drained(), IDLE, mbox("j", "trash"), IDLE, waitR("hasSubject:XF j msg 3", "true"), settle(500), cap("trash-shows-it-unread-unflagged"),
    {"op": "search", "text": "is:unread"}, IDLE, settle(600),
    EL("the message was opened (marked read) before Delete: it must not come back unread", ("reading:hasSubject:XF j msg 3", "false")),
    {"op": "search", "text": "is:flagged"}, IDLE, settle(600),
    EL("the message was flagged before Delete: the flag must survive the move", ("reading:hasSubject:XF j msg 3", "true")),
], timeout=300000)

# --- 10. DEFECT: an archived message is nowhere in the app afterwards ----------------------------
write("xf-defect-archived-mail-vanishes", ["archive", "search"], [
    add("k"), writes("k"), mbox("k", "inbox"), IDLE, msg("XF k msg 3"), IDLE, settle(2500), drained(),
    cmd("archive"), settle(500), drained(), IDLE, settle(6000),
    mbox("k", "archive"), IDLE, settle(1500), cap("archive-mailbox-after-archiving"),
    EL("the Archive mailbox lists the message just archived", ("reading:hasSubject:XF k msg 3", "true")),
    mbox("k", "inbox"), IDLE, R("searchScope", text="account"), {"op": "search", "text": "XF k msg 3"}, IDLE, settle(900), cap("account-search"),
    EL("an account-wide search still finds it", ("reading:hasSubject:XF k msg 3", "true")),
], timeout=300000)
