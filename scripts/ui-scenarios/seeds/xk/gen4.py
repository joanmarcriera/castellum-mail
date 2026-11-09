# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Round 4 scenarios (run from the worktree root): python3 build/explore/xk/gen4.py
import json
S={"width":1280,"height":768,"menuBar":25}
idle={"op":"wait","until":"idle"}; loaded={"op":"wait","until":"accountsLoaded"}
SH="class:SheetPresentationWindow"
def pause(ms): return {"op":"ext","id":"pause","text":str(ms)}
def cap(n,ws=None):
    d={"op":"capture","name":n}
    if ws: d["windows"]=ws
    return [d]
def M(k,v): return {"model":k,"equals":v}
def expect(label,*e,window=None,rules=None):
    d={"op":"assert","label":label,"expect":list(e)}
    if window: d["window"]=window
    if rules: d["rules"]=rules
    return [d]
def acct(p): return {"op":"addAccount","persona":p,"as":p,"waitFor":"synced","timeoutMs":90000}
def popen(): return [{"op":"command","id":"showCommandPalette"},pause(300),idle]
def ptype(t): return [{"op":"reading","id":"fieldText","window":SH,"text":t},idle]
def psubmit(): return [{"op":"accounts","id":"click","window":SH,"text":"100,20"},pause(500),idle]
def run(q): return popen()+ptype(q)+psubmit()
def w(name,steps,covers,req=True,t=180000):
    d={"schema":1,"name":name,"covers":covers,"screen":S,"timeoutMs":t,"steps":steps+[{"op":"quit"}]}
    if req: d["requires"]={"greenmail":True,"personas":True}
    json.dump(d,open(f"scripts/ui-scenarios/{name}.json","w"),indent=1)

# defect: a palette request made while the cheat sheet is up is not dropped; it fires when the sheet is dismissed
st=[loaded,idle,{"op":"command","id":"showShortcuts"},pause(500),idle]+expect("cheat sheet up",{"sheetAttached":True},{"windowCount":2})
st+=[{"op":"command","id":"showCommandPalette"},pause(600),idle]+cap("palette-requested-over-cheat-sheet")
st+=expect("the request does nothing visible: still the one cheat sheet",{"windowCount":2})
st+=[{"op":"accounts","id":"click","window":SH,"text":"572,536","label":"Done"},pause(1500),idle]+cap("after-done")
st+=expect("after Done the main window has no sheet (the stale palette request must not fire now)",{"sheetAttached":False},{"windowCount":1})
w("xk-defect-palette-pops-up-after-cheat-sheet",st,["S45 command palette","cheat sheet","a command that could not run is dropped, not replayed later"],req=False,t=60000)

# pass: the palette drives real work with mail present
st=[loaded,acct("alice"),{"op":"reading","id":"selectMailbox","accountRef":"alice","text":"inbox"},idle,
    {"op":"reading","id":"selectMessage","text":"Invoice R0012345678"},idle]
st+=run("classic")+expect("Classic Layout from the palette",M("reading:layout","classic"),{"sheetAttached":False},M("reading:selectedSubject","Invoice R0012345678 for August 2026"))
st+=run("widescreen")+expect("Widescreen Layout from the palette keeps the open message",M("reading:layout","widescreen"),M("reading:selectedSubject","Invoice R0012345678 for August 2026"))
st+=run("fit columns")+cap("after-fit-columns",["main"])+expect("Fit Columns",M("reading:tableFound","true"),M("reading:errorText","none"),window="main",rules=["nonBlank"])
st+=run("outbox")+cap("outbox-from-palette")+expect("Outbox window from the palette",{"windowCount":2},{"sheetAttached":False})
st+=run("status")+expect("Status window from the palette",{"windowCount":3})
st+=run("new message")+cap("compose-from-palette")+expect("New Message from the palette opens a composer",{"windowCount":4},M("ext:composeWindows","1"),M("ext:pendingCount","0"))
st+=popen()+cap("palette-over-main-with-composer-open")+ptype("send")+psubmit()+cap("send-on-empty-composer")
st+=expect("Send on an empty composer sends nothing",M("ext:pendingCount","0"),M("ext:composeWindows","1"))
w("xk-palette-with-mail",st,["S45 command palette with mail","layout switch keeps selection","windows from the palette","empty composer never sends"])

# explore: signatures window opened twice
st=[loaded,acct("alice"),acct("bob"),{"op":"wait","until":"settingsOpenerReady"},
    {"op":"openWindow","target":"settings","tab":"accounts","accountRef":"bob"},{"op":"wait","until":"window:settings"},
    {"op":"resizeWindow","window":"settings","contentSize":[760,560]},idle,
    {"op":"accounts","id":"click","window":"settings","text":"700,331","name":"signatures"},{"op":"wait","until":"window:signatures"},idle]
st+=cap("signatures-first")+expect("signatures open",{"windowCount":3},window="signatures",rules=["nonBlank","resizable","movable","fitsScreen"])
st+=[{"op":"accounts","id":"click","window":"settings","text":"700,331"},pause(800),idle]+cap("signatures-second-request")
st+=expect("Signatures… twice re-fronts the one window",{"windowCount":3})
st+=[{"op":"resizeWindow","window":"signatures","contentSize":[1240,700]},idle]+cap("signatures-large",["signatures"])+expect("large",window="signatures",rules=["nonBlank"])
st+=[{"op":"closeWindow","window":"settings"},idle]+cap("settings-closed-signatures-stays")+expect("closing Settings leaves the Signatures window usable",{"windowCount":2},window="signatures",rules=["nonBlank"])
st+=[{"op":"closeWindow","window":"signatures"},idle]+expect("closed",{"windowCount":1})
w("xk-signatures-window-twice",st,["signatures window opened twice","large size","outlives Settings"])
