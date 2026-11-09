# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# xe scenarios (executed by gen.py). Seed first: build/explore/xe/run.sh <name>.
NEWS, INV, LIST = group(cond("from", "xe-news")), group(cond("subject", "XE Invoice")), group(cond("from", "xe-list"))

S("xe-rules-eval-mode-refused", ["MM-3", "evaluation mode: every rule action refused, nothing queued"],
  START + [add(R1, "r1"),
   rule("Read news", NEWS, ["mark_read"], "r1"),
   rule("Star invoices", INV, [STAR], "r1"),
   rule("Archive news", NEWS, ["archive"], "r1"),
   rule("Trash list", LIST, ["trash"], "r1"),
   rule("File invoices", INV, ["mark_read", MOVE(R1 + ":Receipts")], "r1"),
   preview("Read news"), expect(note("3")),
   preview("Star invoices"), expect(note("2")),
   run("Read news"), expect(has("evaluation mode"), has("3 matched, 0 actions queued"), ("ext:pendingCount", "0")),
   run("Star invoices"), expect(has("evaluation mode"), has("2 matched, 0 actions queued"), ("ext:pendingCount", "0")),
   run("Archive news"), expect(has("evaluation mode"), has("3 matched, 0 actions queued"), ("ext:pendingCount", "0")),
   run("Trash list"), expect(has("evaluation mode"), has("0 actions queued"), ("ext:pendingCount", "0")),
   run("File invoices"), expect(has("evaluation mode"), has("2 matched, 0 actions queued"), ("ext:pendingCount", "0")),
   mailbox("r1", "inbox"), idle(),
   expect(("reading:messageCount", "8"), ("reading:errorText", "none")),
   cap("inbox-untouched", ["main"])]
  + rules_tab() + [cap("rules-tab-read-only", ["settings"]),
   expect(("reading:ruleCount", "5"), window="settings", rules=["nonBlank", "fitsScreen"])] + QUIT)


Q = lambda: expect(note("?"))   # probe: the violation text shows the real notice
def flush(t=45000): return [{"op": "reading", "id": "reSyncAll"}, waitext("pendingCount", "0", t), idle()]
S("xx-probe-writes", ["probe"],
  START + [add(R1, "r1"), add(R2, "r2"), writes("r1"),
   rule("Read news", NEWS, ["mark_read"], "r1"),
   rule("Star invoices", INV, [STAR], "r1"),
   rule("Trash list", LIST, ["trash"], "r1"),
   rule("Archive news", NEWS, ["archive"], "r1"),
   rule("File invoices", INV, ["mark_read", MOVE(R1 + ":Receipts")], "r1"),
   rule("Gone folder", group(cond("from", "xe-corp")), ["mark_read", MOVE(R1 + ":Nonexistent")], "r1"),
   rule("Cross account", NEWS, [MOVE(R1 + ":Receipts")], "r2"),
   rule("Star all", group(cond("from", "xe-")), [STAR]),
   rule("Nothing", group(cond("from", "nobody-xyz")), ["mark_read"], "r1"),
   preview("Read news"), Q(), run("Read news"), Q()] + flush() + [
   preview("Read news"), Q(), run("Read news"), Q(), expect(("ext:pendingCount", "?"))] + flush() + [
   run("Star invoices"), Q(),
   preview("Trash list"), Q(), run("Trash list"), Q()] + flush() + [
   preview("Trash list"), Q(),
   run("Archive news"), Q()] + flush() + [preview("Archive news"), Q(), run("Archive news"), Q(),
   preview("File invoices"), Q(), run("File invoices"), Q()] + flush() + [
   preview("File invoices"), Q(), run("File invoices"), Q()] + flush() + [
   preview("Gone folder"), Q(), run("Gone folder"), Q(),
   preview("Cross account"), Q(), run("Cross account"), Q(),
   preview("Star all"), Q(), run("Star all"), Q(),
   preview("Nothing"), Q(), run("Nothing"), Q(),
   {"op": "reading", "id": "toggleRule", "name": "Star invoices", "value": False}, run("Star invoices"), Q()] + flush() + [
   mailbox("r1", "inbox"), idle(), expect(("reading:subjects", "?")), cap("inbox-after", ["main"])]
  + rules_tab() + [cap("rules-tab", ["settings"])] + QUIT, timeout=400000)

S("xx-probe-editor", ["probe"],
  START + [add(R1, "r1"), add(R2, "r2")] + rules_tab() + [
   cap("empty-rules", ["settings"]),
   click("settings", 45, 422, "sheet:ruleEditor"), {"op": "wait", "until": "sheet:ruleEditor"}, idle(),
   cap("editor-new"),
   {"op": "ext", "id": "describe", "window": "sheet:ruleEditor"}] + QUIT)

ALLXE = group(cond("from", "xe-"))
NOTHING = group(cond("from", "nobody-xyz"))
EVAL = "evaluation mode"

S("xe-rules-run-now-writes", ["MM-3", "Run Now with writes enabled: counts, protected folders, match nothing / everything"],
  START + [add(R1, "r1"), writes("r1"),
   rule("Nothing", NOTHING, ["mark_read"], "r1"),
   rule("Everything", ALLXE, [STAR], "r1"),
   rule("Read news", NEWS, ["mark_read"], "r1"),
   rule("File invoices", INV, ["mark_read", MOVE(R1 + ":Receipts")], "r1"),
   rule("Archive news", NEWS, ["archive"], "r1"),
   preview("Nothing"), expect(note("0")), run("Nothing"), expect(note("0 matched, 0 actions queued."), ("ext:pendingCount", "0")),
   preview("Read news"), expect(note("3")),   # the copy in Trash is never a candidate
   run("Read news"), expect(note("3 matched, 3 actions queued."))] + flush() + [
   run("File invoices"), expect(note("2 matched, 4 actions queued."))] + flush() + [
   run("Archive news"), expect(note("3 matched, 3 actions queued."))] + flush() + [
   mailbox("r1", "inbox"), idle(),
   expect(("reading:messageCount", "3"), ("reading:hasSubject:XE Weekly", "false"), ("reading:hasSubject:XE Invoice", "false"),
          ("reading:hasSubject:XE Long", "true"), ("reading:errorText", "none")),
   cap("inbox-after-rules", ["main"])]
  + rules_tab() + [cap("rules-tab-writes-on", ["settings"]), expect(("reading:ruleCount", "5"), window="settings", rules=["nonBlank"])] + QUIT,
  timeout=300000)

S("xe-rules-two-accounts", ["MM-3", "all-accounts rule with one read-only account; per-account scope; refusal counts"],
  START + [add(R1, "r1"), add(R2, "r2"), writes("r1"),
   rule("Star all", ALLXE, [STAR]),
   rule("R2 read news", NEWS, ["mark_read"], "r2"),
   run("R2 read news"), expect(has("2 matched, 0 actions queued"), has(EVAL), ("ext:pendingCount", "0")),
   run("Star all"), expect(has(" actions queued. Some messages could not be changed: " + EVAL), has("3 of "))] + flush() + rules_tab() + [
   cap("banner-names-only-r2", ["settings"]),
   writes("r2"), idle(),
   run("R2 read news"), expect(note("2 matched, 2 actions queued."))] + flush() + [
   cap("banner-gone-after-enable", ["settings"]),
   mailbox("r1", "inbox"), idle(), expect(("reading:messageCount", "8"), ("reading:errorText", "none"))] + QUIT, timeout=300000)

LONG = "A very long rule name that a person might paste by accident " * 4
S("xe-rules-list-order-disable", ["MM-3", "rules list: create, reorder, disable, long name, duplicate name, smallest window"],
  START + [add(R1, "r1"),
   rule("Alpha", NEWS, ["mark_read"], "r1"), rule("Beta", INV, [STAR], "r1", stop=True), rule("Gamma", LIST, ["trash"]),
   rule(LONG.strip(), NEWS, ["mark_read", STAR, "archive"], "r1"),
   expect(("reading:ruleCount", "4"), ("reading:ruleNames", "Alpha,Beta,Gamma," + LONG.strip())),
   {"op": "reading", "id": "moveRuleToTop", "name": "Gamma"}, idle(),
   {"op": "reading", "id": "toggleRule", "name": "Beta", "value": False}, idle(),
   expect(("reading:ruleNames", "Gamma,Alpha,Beta," + LONG.strip()), ("reading:ruleEnabled:Beta", "false"), ("reading:ruleEnabled:Gamma", "true"))]
  + rules_tab() + [cap("list-reordered", ["settings"]),
   expect(window="settings", rules=["nonBlank", "fitsScreen", "resizable"]),
   {"op": "resizeWindow", "window": "settings", "contentSize": "min"}, idle(), cap("list-smallest-window", ["settings"]),
   expect(window="settings", rules=["nonBlank", "fitsScreen"]),
   {"op": "reading", "id": "toggleRule", "name": "Beta", "value": True}, idle(),
   expect(("reading:ruleEnabled:Beta", "true"), ("reading:errorText", "none"))] + QUIT)

S("xe-rules-missing-folder-run-now", ["MM-3", "rule whose Move folder is gone: Run Now reports it and queues nothing (no half-applied actions)"],
  START + [add(R1, "r1"), writes("r1"),
   rule("Gone folder", group(cond("from", "xe-corp")), ["mark_read", MOVE(R1 + ":Nonexistent")], "r1"),
   preview("Gone folder"), expect(note("1")),
   run("Gone folder"), expect(has("1 matched, 0 actions queued"), has("no longer exists in this account"), ("ext:pendingCount", "0")),
   mailbox("r1", "inbox"), idle(), expect(("reading:messageCount", "8"), ("reading:errorText", "none"))]
  + rules_tab() + [cap("rule-row-shows-raw-mailbox-id", ["settings"])] + QUIT)

S("xe-defect-run-now-actions-not-sent", ["MM-3", "DEFECT: Run Now queues actions but never starts the replay; they wait for the next sync (5-15 min)"],
  START + [add(R1, "r1"), writes("r1"), rule("Read news", NEWS, ["mark_read"], "r1"),
   run("Read news"), expect(note("3 matched, 3 actions queued.")),
   settle(20000), mailbox("r1", "inbox"), idle(), cap("twenty-seconds-after-run-now", ["main"]),
   # Every other write (archive, star, mark read) is replayed ~1.5 s later via scheduleReplay().
   expect(("ext:pendingCount", "0"))] + QUIT)

S("xe-defect-move-folder-of-other-account", ["MM-3", "DEFECT: a rule scoped to account B with Move to a folder of account A is accepted (editor: change Apply to after Add Action > Move to)"],
  START + [add(R1, "r1"), add(R2, "r2"),
   # RuleDraft.validationProblem and upsert_filter both accept this; the row then reads "xe-r2 -> Move to Receipts".
   rule("Cross account", NEWS, [MOVE(R1 + ":Receipts")], "r2")]
  + rules_tab() + [cap("saved-rule-looks-valid", ["settings"]),
   # Correct behaviour: the save is refused (after the fix the saveRule step above errors with the validation text).
   expect(("reading:ruleCount", "0"))] + QUIT)

S("xe-defect-run-now-skips-same-folder-duplicate", ["MM-3", "R1", "DEFECT: two Inbox messages sharing a Message-ID: preview and Run Now count and act on one only"],
  START + [add(R1, "r1"), writes("r1"), rule("Trash list", LIST, ["trash"], "r1"),
   mailbox("r1", "inbox"), idle(),
   preview("Trash list"), expect(note("2")),
   run("Trash list")] + flush() + [mailbox("r1", "inbox"), idle(), cap("one-copy-left-in-inbox", ["main"]),
   expect(("reading:hasSubject:XE Dup notice", "false"))] + QUIT)

S("xe-defect-run-now-requeues-done-work", ["MM-3", "DEFECT: a second Run Now queues one server operation per message again although nothing would change"],
  START + [add(R1, "r1"), writes("r1"), rule("Read news", NEWS, ["mark_read"], "r1"),
   run("Read news"), expect(note("3 matched, 3 actions queued."))] + flush() + [
   run("Read news"), cap("second-run", ["main"]),
   expect(has("0 actions queued"), ("ext:pendingCount", "0"))] + QUIT)

S("xx-probe-editor2", ["probe"],
  START + [add(R1, "r1"), add(R2, "r2")] + rules_tab() + [
   click("settings", 45, 422, "sheet:ruleEditor"), {"op": "wait", "until": "sheet:ruleEditor"}, idle(),
   {"op": "accounts", "id": "setNth", "window": "sheet:ruleEditor", "name": "0", "text": "Editor news"},
   {"op": "accounts", "id": "popupSelect", "window": "sheet:ruleEditor", "find": {"role": "popup", "label": "All accounts"}, "text": R1},
   {"op": "accounts", "id": "setNth", "window": "sheet:ruleEditor", "name": "1", "text": "XE Weekly"},
   {"op": "accounts", "id": "popupSelect", "window": "sheet:ruleEditor", "find": {"role": "popup", "label": "Add Action"}, "text": "Mark as read"},
   idle(), cap("editor-filled", ["sheet:ruleEditor"]),
   {"op": "accounts", "id": "popupSelect", "window": "sheet:ruleEditor", "find": {"role": "popup", "label": "Subject"}, "text": "?"}] + QUIT)

UNREAD_FILED = {"op": "and", "conditions": [cond("subject", "XE Invoice 100"), {"field": "is_read", "operator": "is", "value": False}], "groups": []}
S("xe-defect-rule-mark-read-lost-on-move", ["MM-3", "DEFECT: in a 'Mark as read + Move' rule the mark-read never reaches the server (replay finds the local row already deleted by the move and treats the op as moot)"],
  START + [add(R1, "r1"), writes("r1"),
   rule("File invoices", INV, ["mark_read", MOVE(R1 + ":Filed")], "r1"),
   rule("Probe unread filed", UNREAD_FILED, [STAR], "r1"),   # never run: its preview counts unread filed invoices
   preview("Probe unread filed"), expect(note("2")),
   run("File invoices"), expect(note("2 matched, 4 actions queued."))] + flush() + [
   mailbox("r1", "Filed"), waitfor("hasSubject:XE Invoice 1001", "true", 45000), idle(), settle(1500),
   cap("filed-invoices-still-unread", ["main"]),
   preview("Probe unread filed"),
   # Correct: both filed invoices are read (0 unread). The server copy never got \\Seen, so the re-synced rows are unread.
   expect(note("0"))] + QUIT, timeout=240000)

ED = "sheet:ruleEditor"
def nth(n, text): return {"op": "accounts", "id": "setNth", "window": ED, "name": str(n), "text": text}
def popup(label, item): return {"op": "accounts", "id": "popupSelect", "window": ED, "find": {"role": "popup", "label": label}, "text": item}
OPEN_EDITOR = [click("settings", 45, 422, ED), {"op": "wait", "until": "sheet:ruleEditor"}, idle()]
S("xe-rules-editor-cancel", ["MM-3", "rule editor: empty state, validation hints, very long name, cancel half-way leaves no rule"],
  START + [add(R1, "r1"), add(R2, "r2")] + rules_tab() + [cap("rules-empty-state", ["settings"])] + OPEN_EDITOR + [
   cap("editor-empty", [ED]),
   expect(window=ED, rules=["nonBlank", "fitsScreen", "noZeroSizeTextFields"]),
   nth(0, LONG.strip()), popup("All accounts", R1), nth(1, "XE Weekly"), idle(),
   cap("editor-half-filled-long-name", [ED]),
   expect(window=ED, rules=["nonBlank", "fitsScreen", "noZeroSizeTextFields"]),
   click(ED, 524, 379), {"op": "wait", "until": "noSheet:ruleEditor"}, idle(),
   expect(("reading:ruleCount", "0"), ("reading:errorText", "none")),
   cap("after-cancel", ["settings"])] + QUIT)

S("xe-defect-rule-moves-into-unsynced-folder", ["MM-3", "DEFECT: Move to... offers a folder Castellum does not sync (unsubscribed); after the rule runs the mail is in neither the Inbox nor the destination list"],
  START + [add(R1, "r1"), writes("r1"),
   rule("File invoices", INV, [MOVE(R1 + ":Receipts")], "r1"),
   run("File invoices"), expect(note("2 matched, 2 actions queued."))] + flush() + [
   mailbox("r1", "inbox"), idle(), expect(("reading:hasSubject:XE Invoice 1001", "false")),
   mailbox("r1", "Receipts"), settle(10000), idle(), cap("destination-folder-empty", ["main"]),
   # The server's Receipts now holds 3 messages (see the wrapper's dump); the app lists none.
   expect(("reading:hasSubject:XE Invoice 1001", "true"))] + QUIT, timeout=240000)
