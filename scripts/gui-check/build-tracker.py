#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Build the GUI-check tracker page (a claude.ai Artifact with a db-backed Pass/Fail per check).

Usage: scripts/gui-check/build-tracker.py <pack.md> <out.html>
  pack.md  = project-docs/gui-check-pack-*.md
Parses sections (## N. Title; section 0 is the bootstrap block), checks
(- [ ] **S12 — title** or **PR-163 — title**), an optional italic note, numbered steps,
**Expect:**/**Recommendation:** lines and free paragraphs, plus the sign-off table's links,
and inlines them as JSON into template.html's /*__DATA__*/ slot. Publish the output with the
Artifact tool (capabilities: db); Claude reads verdicts back via ArtifactData collection "checks".
"""
import json, pathlib, re, sys

md_path, out_path = sys.argv[1:3]
tpl_path = pathlib.Path(__file__).with_name("template.html")
lines = open(md_path, encoding="utf-8").read().splitlines()
ID = r"(S[0-9A-Za-z-]+|PR-\d+)"

bootstrap, sections, links = [], [], {}
in_code = in_boot = False
cur_sec = cur = None
for raw in lines:
    line = raw.rstrip()
    if line.startswith("```"):
        in_code = not in_code
        continue
    if in_code:
        if in_boot:
            bootstrap.append(line)
        continue
    m = re.match(rf"^\| {ID} \| (https://\S+) \|", line)
    if m:
        links[m.group(1)] = m.group(2)
        continue
    m = re.match(r"^## (\d+)\. (.+)$", line)
    if m and m.group(1) == "0":
        in_boot, cur_sec, cur = True, None, None
        continue
    if m:
        in_boot = False
        cur_sec = {"n": int(m.group(1)), "title": m.group(2), "stages": []}
        sections.append(cur_sec)
        cur = None
        continue
    if line.startswith("## "):
        cur_sec = cur = None
        continue
    m = re.match(rf"^- \[ \] \*\*{ID} — (.+?)\*\*\s*$", line)
    if m and cur_sec is not None:
        cur = {"id": m.group(1), "title": m.group(2), "note": "", "steps": [], "expect": "", "text": []}
        cur_sec["stages"].append(cur)
        continue
    if cur is None:
        continue
    s = line.strip()
    if not s or s == "---":
        continue
    m = re.match(r"^(\d+)\. (.+)$", s)
    if m:
        cur["steps"].append(m.group(2))
    elif s.startswith("**Expect:**"):
        cur["expect"] = s[len("**Expect:**"):].strip()
    elif s.startswith("**Recommendation:**"):
        cur["expect"], cur["expectLabel"] = s[len("**Recommendation:**"):].strip(), "Recommendation"
    elif s.startswith("*") and s.endswith("*") and not s.startswith("**"):
        cur["note"] = s.strip("*")
    elif cur["steps"]:
        cur["steps"][-1] += " " + s
    else:
        cur["text"].append(s)

for sec in sections:
    for st in sec["stages"]:
        st["tracker"] = links.get(st["id"], "")
count = sum(len(s["stages"]) for s in sections)
assert count >= 20, f"parsed only {count} checks"
data = {"bootstrap": "\n".join(bootstrap).strip(), "sections": sections, "source": md_path}
payload = json.dumps(data, ensure_ascii=False).replace("</", "<\\/")
out = tpl_path.read_text(encoding="utf-8").replace("/*__DATA__*/null", payload)
pathlib.Path(out_path).write_text(out, encoding="utf-8")
print(f"{count} checks in {len(sections)} sections -> {out_path}")
