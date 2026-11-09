# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Print probe results of a run next to the query that produced them. usage: probe.py <scenario.json> <run dir>"""
import json, sys, re
sc = json.load(open(sys.argv[1])); rep = json.load(open(sys.argv[2] + "/report.json"))
vs = [v for v in rep.get("violations", [])]
labels = []
last = "-"
for s in sc["steps"]:
    if s["op"] == "search": last = "Q[" + s["text"] + "]"
    elif s["op"] == "reading": last = s["id"] + ":" + str(s.get("text", s.get("name", "")))
    if s["op"] == "assert":
        for e in s.get("expect", []):
            if e.get("equals") == "?": labels.append(last)
i = 0
for v in vs:
    d = v["detail"]
    m = re.match(r"(reading:.+?) = '(.*)', expected '\?'$", d, re.S)
    if m:
        print(f"{(labels[i] if i < len(labels) else "??")[:34]:34s} {m.group(1).replace('reading:',''):14s} {m.group(2)[:int(sys.argv[3]) if len(sys.argv)>3 else 230]}"); i += 1
    else: print("!!", v["rule"], v["element"], d[:300])
