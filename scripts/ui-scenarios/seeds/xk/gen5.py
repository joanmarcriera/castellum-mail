# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Generates the MM-12 slice 1 key-sequence scenarios and their keybindings seeds (run from the worktree root).
# Usage: python3 scripts/ui-scenarios/seeds/xk/gen5.py ; then, per scenario with a seed,
#   scripts/ui-scenarios/seeds/xk/run-kb.sh <scenario> build/explore/xk/kb-<x>.json
# (the harness has no "seed a profile file" op; run-kb.sh copies the file into the run's temp profile).
import json, os
S = {"width": 1280, "height": 768, "menuBar": 25}
idle = {"op": "wait", "until": "idle"}
def pause(ms): return {"op": "ext", "id": "pause", "text": str(ms)}
def rd(id_, **kw): return dict({"op": "reading", "id": id_}, **kw)
def keys(text): return [{"op": "ext", "id": "keys", "window": "main", "text": text}, pause(400), idle]
def expect(label, **kv):
    return {"op": "assert", "label": label, "expect": [{"model": "reading:" + k, "equals": v} for k, v in kv.items()]}
def w(name, steps, covers, mail=True):
    json.dump({"schema": 1, "name": name, "covers": covers, "requires": {"greenmail": True, "personas": True} if mail else {},
               "screen": S, "timeoutMs": 240000, "steps": steps + [{"op": "quit"}]},
              open(f"scripts/ui-scenarios/{name}.json", "w"), indent=1, ensure_ascii=False)
    if not mail: pass
def seed(n, version, ov):
    os.makedirs("build/explore/xk", exist_ok=True)
    json.dump({"version": version, "overrides": ov}, open(f"build/explore/xk/kb-{n}.json", "w"), ensure_ascii=False, indent=1)
login = [{"op": "wait", "until": "accountsLoaded"},
         {"op": "addAccount", "persona": "alice", "as": "alice", "waitFor": "synced", "timeoutMs": 90000},
         rd("selectMailbox", accountRef="alice", text="inbox"), idle,
         rd("selectMessage", text="Invoice R0012345678"), idle]

# seeded file: a three-key-free override set; sequences and a rebound nextMessage
seed("seq", 2, {"gotoSent": {"keys": ["⌃x s"]}, "nextMessage": {"keys": ["n"]}})
st = login + [pause(2000), rd("reloadKeyBindings"), idle,
              expect("the file loaded without problems", keyProblems="none")]
st += keys("⌃x s") + [rd("waitFor", name="mailboxRole", text="sent", timeoutMs=15000)]
st += keys("g i") + [rd("waitFor", name="mailboxRole", text="inbox", timeoutMs=15000)]
st += keys("g s") + [pause(300), expect("g s no longer jumps to Sent (keys replaced the chord)", mailboxRole="inbox")]
st += [{"op": "ext", "id": "selectRows", "text": "0"}, idle, expect("row 0 selected", selectedIndex="0")]
st += keys("n") + [expect("n (nextMessage rebound by keys) moved the selection", selectedIndex="1"), {"op": "capture", "name": "after-n"}]
st += keys("j") + [pause(300), expect("j no longer moves (keys replaced the bare key)", selectedIndex="1")]
w("xk-keys-sequence-override", st, ["MM-12 slice 1", "a user sequence ⌃x s selects Sent", "nextMessage rebound to n", "g i still selects Inbox"])

# reload without relaunch: the file is copied in at launch; the scenario proves j works, then reloads and n takes over
seed("reload", 2, {"nextMessage": {"keys": ["n"]}})
st = login + [{"op": "ext", "id": "selectRows", "text": "0"}, idle]
st += [pause(2000), rd("reloadKeyBindings"), idle, expect("reload applied without a problem", keyProblems="none")]
st += keys("n") + [expect("n moves after the reload, no relaunch", selectedIndex="1"), {"op": "capture", "name": "after-reload-n"}]
w("xk-keys-reload-without-relaunch", st, ["MM-12 slice 1", "Reload Key Bindings reaches bare keys without a relaunch"])

# future version: defaults plus one named problem
seed("future", 3, {"archive": {"keys": ["y"]}})
st = login + [pause(2000), rd("reloadKeyBindings"), idle,
              expect("the problem names version 3", keyProblems="unsupportedVersion", keyProblemText="keybindings.json has version 3, but this version of Castellum reads versions 1 to 2 — using the built-in key bindings.")]
w("xk-keys-future-version", st, ["MM-12 slice 1", "a version 3 file loads the defaults and says so"])
