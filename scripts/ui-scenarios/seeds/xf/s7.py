# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf UIT-1 sort scenarios. Usage: python3 scripts/ui-scenarios/seeds/xf/seed_sort.py && python3 scripts/ui-scenarios/seeds/xf/s7.py
Writes scripts/ui-scenarios/xf-list-sort-by-column.json: sort the Inbox by From, Subject and Received in both
directions through the model (`sortBy`; header presses are not drivable, #3158) and assert the order, the
table's own sort indicator, and that the choice survives a reload."""
import os, gen
gen.ROOT = os.path.abspath(os.path.dirname(os.path.abspath(__file__)) + "/../../../..")
from gen import *
def lab(step, text): step["label"] = text; return step
def EL(text, *pairs): return lab(E(*pairs), text)
def sortby(name, direction): return R("sortBy", name=name, text=direction)
def at(mark): return {"op": "capture", "name": mark}
ADD = {"op": "addAccount", "email": "xf-s@local.test", "password": "xf-s@local.test", "as": "s", "waitFor": "synced", "timeoutMs": 90000}
FIRST = "reading:firstRowSubjects"
write("xf-list-sort-by-column", ["sort", "list", "headers"], [
    ADD, mbox("s", "inbox"), IDLE, settle(1500),
    EL("default: newest first, Received arrow down", (FIRST, "Dates|Fwd: carrots|Émile letter|Banana bread|apple pie"),
       ("reading:sort", "date:desc"), ("reading:sortIndicator", "date:desc")),
    sortby("from", "asc"), IDLE, settle(800), at("from-asc"),
    EL("From A-Z folds accents and case; equal senders newest first",
       (FIRST, "apple pie|Dates|Fwd: carrots|Banana bread|Émile letter"),
       ("reading:sort", "from:asc"), ("reading:sortIndicator", "from:asc")),
    sortby("from", "desc"), IDLE, settle(800),
    EL("From Z-A reverses the sender, not the date tie-break",
       (FIRST, "Re: Zebra crossing|Émile letter|Banana bread|Dates|Fwd: carrots"), ("reading:sort", "from:desc")),
    sortby("subject", "asc"), IDLE, settle(800), at("subject-asc"),
    EL("Subject A-Z ignores Re:/Fwd: and accents",
       (FIRST, "apple pie|Banana bread|Fwd: carrots|Dates|Émile letter"), ("reading:sortIndicator", "subject:asc")),
    sortby("date", "asc"), IDLE, settle(800),
    EL("Received oldest first", (FIRST, "Re: Zebra crossing|apple pie|Banana bread|Émile letter|Fwd: carrots"),
       ("reading:sortIndicator", "date:asc")),
    sortby("subject", "asc"), IDLE, settle(500),
    R("reSyncAll"), settle(3000), IDLE,
    EL("a sync reload keeps the chosen sort", (FIRST, "apple pie|Banana bread|Fwd: carrots|Dates|Émile letter"),
       ("reading:sort", "subject:asc")),
    sortby("date", "desc"), IDLE, settle(800),
    EL("back to newest first", (FIRST, "Dates|Fwd: carrots|Émile letter|Banana bread|apple pie"), ("reading:sort", "date:desc")),
    sortby("unread", "asc"), IDLE, settle(800), at("unread-asc"),
    EL("the flag-style keys sort too (slice 5); all seeded mail is unread, so the date order inside the group shows",
       (FIRST, "Dates|Fwd: carrots|Émile letter|Banana bread|apple pie"), ("reading:sortIndicator", "unread:asc")),
    sortby("starred", "desc"), IDLE, settle(500),
    EL("Flag", ("reading:sortIndicator", "starred:desc")),
    sortby("attachments", "desc"), IDLE, settle(500),
    EL("Attachments", ("reading:sortIndicator", "attachments:desc")),
], timeout=240000)

# Slice 4: a search opens in relevance order (no arrow); a header click sorts the results client-side.
write("xf-list-sort-search", ["sort", "search", "relevance"], [
    ADD, mbox("s", "inbox"), IDLE, settle(1500),
    sortby("subject", "asc"), IDLE, settle(500),
    R("drillDown", name="from", text="sort.test"), IDLE, settle(1500),
    EL("a new search shows relevance order: no sort arrow", ("reading:sortIndicator", "none"), ("reading:messageCount", "6")),
    sortby("from", "desc"), IDLE, settle(1500), at("search-from-desc"),
    EL("a header click sorts the results; equal senders newest first",
       (FIRST, "Re: Zebra crossing|\u00c9mile letter|Banana bread|Dates|Fwd: carrots"),
       ("reading:sortIndicator", "from:desc")),
], timeout=240000)
