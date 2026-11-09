# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Generates the xk key-bindings scenarios and their seed files (run from the worktree root).
# Usage: python3 build/explore/xk/gen2.py ; then build/explore/xk/run-kb.sh <name> build/explore/xk/kb-<x>.json
import json
S={"width":1280,"height":768,"menuBar":25}
idle={"op":"wait","until":"idle"}
AL="class:_NSAlertPanel"
def pause(ms): return {"op":"ext","id":"pause","text":str(ms)}
def cap(n,ws=None):
    d={"op":"capture","name":n}
    if ws: d["windows"]=ws
    return d
def key(k): return [{"op":"ext","id":"keyEquivalent","window":"main","text":k},pause(600),idle]
def w(name,steps,covers):
    json.dump({"schema":1,"name":name,"covers":covers,"screen":S,"timeoutMs":90000,"steps":steps+[{"op":"quit"}]},
              open(f"scripts/ui-scenarios/{name}.json","w"),indent=1)
def seed(n,ov): json.dump({"version":1,"overrides":ov},open(f"build/explore/xk/kb-{n}.json","w"),ensure_ascii=False,indent=1)
pre=[{"op":"wait","until":"accountsLoaded"},pause(8000),idle,{"op":"wait","until":"settingsOpenerReady"},
     {"op":"openWindow","target":"settings","tab":"general"},{"op":"wait","until":"window:settings"},idle,
     {"op":"resizeWindow","window":"settings","contentSize":[760,1100]},idle,
     {"op":"accounts","id":"click","window":"settings","text":"440,856","label":"Reload Key Bindings (a SwiftUI button: no NSControl to press)"},pause(800),cap("after-reload")]
def alert(text,label): return {"op":"accounts","id":"waitNode","window":AL,"find":{"text":text},"value":True,"timeoutMs":4000,"label":label}
ok={"op":"press","window":AL,"find":{"label":"OK"}}
sheet=[{"op":"command","id":"showShortcuts"},pause(600),idle,cap("cheat-sheet")]

# pass: a valid override applies, a colliding one is refused with a message
seed("ok",{"showStatus":{"menuChord":"⇧⌘9"},"showDiagnostics":{"menuChord":"⇧⌘K"}})
st=pre+[alert("showDiagnostics","the override that collides with the palette's shift-cmd-K is refused and named"),ok,pause(500),idle,
        {"op":"closeWindow","window":"settings"},idle,{"op":"assert","label":"only the main window","expect":[{"windowCount":1}]}]
st+=key("cmd+shift+9")+[cap("status-by-custom-chord"),{"op":"assert","label":"the custom chord shift-cmd-9 opens Status","expect":[{"windowCount":2}]}]
st+=key("cmd+shift+k")+[cap("palette-still-on-its-chord"),{"op":"assert","label":"shift-cmd-K still opens the palette","expect":[{"sheetAttached":True}]}]
w("xk-keybindings-custom-override",st,["S45 custom key bindings","valid override applies","colliding override refused with a message"])

# defect: a chord that cannot be parsed is dropped without a word
seed("typo",{"showStatus":{"menuChord":"⇧⌘9"},"showDiagnostics":{"menuChord":"cmd+shift+8"},"showOutbox":{"bareKey":"oo"}})
st=pre+[{"op":"closeWindow","window":"settings"},pause(300)]+key("cmd+shift+9")
st+=[cap("control-valid-override-applied"),{"op":"assert","label":"control: the file was read (the valid shift-cmd-9 override opens Status)","expect":[{"windowCount":2}]},
     alert("showDiagnostics","Reload tells the user that the showDiagnostics chord 'cmd+shift+8' could not be understood")]
w("xk-defect-keybinding-typo-silent",st,["S45 custom key bindings","KeyBindingsStore.warnings: an unparseable chord is reported"])

# defect: a chord macOS/the app already owns is accepted
seed("reserved",{"showStatus":{"menuChord":"⌘,"},"showOutbox":{"menuChord":"⇧⌘9"}})
st=pre+[{"op":"closeWindow","window":"settings"},pause(300)]+key("cmd+shift+9")
st+=[cap("control-outbox-by-custom-chord"),{"op":"assert","label":"control: the file was read (shift-cmd-9 opens the Outbox)","expect":[{"windowCount":2}]}]
st+=key("cmd+,")+[cap("after-cmd-comma-settings-not-status"),
     {"op":"assert","label":"cmd-comma still opens Settings (3 windows: main, Outbox, Settings), so the accepted 'Status = cmd-comma' binding is dead","expect":[{"windowCount":3}],"window":"title:Outbox","rules":["movable"]},
     alert("showStatus","Reload refuses to bind Status to cmd-comma (Settings) and says so")]
w("xk-defect-keybinding-reserved-chord",st,["S45 custom key bindings","an override may not collide with another command"])
