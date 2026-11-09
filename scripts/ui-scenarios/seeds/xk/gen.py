# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Generates the xk-* UI scenarios (explorer journey: windows, keyboard, command palette).
# Usage: python3 build/explore/xk/gen.py   (from the worktree root; only writes scripts/ui-scenarios/xk-*.json)
import json
S={"width":1280,"height":768,"menuBar":25}
def w(name,steps,covers,req=False,t=180000):
    d={"schema":1,"name":name,"covers":covers,"screen":S,"timeoutMs":t,"steps":steps+[{"op":"quit"}]}
    if req: d["requires"]={"greenmail":True,"personas":True}
    json.dump(d,open(f"scripts/ui-scenarios/{name}.json","w"),indent=1)
idle={"op":"wait","until":"idle"}
loaded={"op":"wait","until":"accountsLoaded"}
def alice(): return [loaded,{"op":"addAccount","persona":"alice","as":"alice","waitFor":"synced","timeoutMs":90000},
    {"op":"reading","id":"selectMailbox","accountRef":"alice","text":"inbox"},idle]
def palette(): return [{"op":"reading","id":"showPalette"},{"op":"wait","until":"sheet:palette"},idle]
def ptype(t): return [{"op":"reading","id":"fieldText","window":"sheet:palette","text":t},idle]
def pclick(row=0): return [{"op":"accounts","id":"click","window":"sheet:palette","text":"200,%d"%(61+33*row)},idle]
def cap(n,ws=None):
    d={"op":"capture","name":n}
    if ws: d["windows"]=ws
    return [d]
def expect(label,*e,window=None,rules=None):
    d={"op":"assert","label":label,"expect":list(e)}
    if window: d["window"]=window
    if rules: d["rules"]=rules
    return [d]
def M(k,v): return {"model":k,"equals":v}

# A: aux windows on an empty profile
CMD={"status":"showStatus","aiActivity":"showAiActivity","diagnostics":"showDiagnostics","outbox":"showOutbox","contacts":"showContacts","projects":"showProjects"}
st=[loaded,idle]+cap("main-empty")
for t,c in CMD.items():
    st+=[{"op":"openWindow","target":t},{"op":"wait","until":"window:"+t},idle]+cap(t+"-default",[t])
    st+=expect(t+" default",{"windowCount":2},window=t,rules=["nonBlank","resizable","movable","fitsScreen","onScreen"])
    st+=[{"op":"command","id":c},idle]+expect(t+" opened twice stays one window",{"windowCount":2})
    st+=[{"op":"resizeWindow","window":t,"contentSize":"min"},idle]+cap(t+"-min",[t])
    st+=expect(t+" min",window=t,rules=["nonBlank","fitsScreen"])   # node rules see 0 nodes in pure-SwiftUI windows
    st+=[{"op":"resizeWindow","window":t,"contentSize":[1240,700]},idle]+cap(t+"-large",[t])
    st+=expect(t+" large",window=t,rules=["nonBlank"])
    st+=[{"op":"closeWindow","window":t},idle]+expect(t+" closed",{"windowCount":1})
w("xk-aux-windows-empty",st,["aux windows empty state","open twice","min and large size","close"])

# A synthetic click on the palette's text field is performClick on the NSTextField = Return (onSubmit).
def psubmit(): return [{"op":"accounts","id":"click","window":"sheet:palette","text":"100,20"},{"op":"ext","id":"pause","text":"400"},idle]

# B: palette on an empty profile
st=[loaded,idle]+palette()+cap("palette-empty-profile")
st+=expect("palette",window="sheet:palette",rules=["nonBlank","fitsScreen"])
st+=[{"op":"command","id":"showCommandPalette"},idle]+expect("palette twice = one sheet",{"windowCount":2},{"sheetAttached":True})
for q,n in [("zzzzqqqq","no-match"),("status "*60,"very-long-query"),("","cleared"),("reply","reply-disabled")]:
    st+=ptype(q)+cap("palette-"+n)
st+=psubmit()+expect("Return on a disabled row (Reply, nothing selected) keeps the palette open",{"sheetAttached":True},{"windowCount":2})
st+=ptype("classic")+cap("palette-classic")+psubmit()
st+=expect("Return ran Classic Layout and closed the palette",{"windowCount":1},{"sheetAttached":False},M("reading:layout","classic"),M("reading:errorText","none"))
st+=cap("after-classic")
st+=palette()+ptype("status")+psubmit()+cap("status-from-palette")
st+=expect("Status opened from the palette",{"windowCount":2},{"sheetAttached":False},window="title:Status")
w("xk-palette-empty-profile",st,["S45 command palette","empty profile","no match","long query","open twice","Return runs command","disabled row inert"])

# C (defect): the palette lists commands by their help text but searches their titles
st=[loaded,idle]+palette()+ptype("classic")+cap("typed-classic-row-does-not-say-classic")
st+=ptype("above")+cap("typed-above-no-match")+psubmit()
st+=expect("typing a word of the visible row label 'Message list above the reading pane' finds and runs it",M("reading:layout","classic"),{"sheetAttached":False})
w("xk-defect-palette-search-ignores-shown-label",st,["S45 command palette","search matches the text the row shows"],t=60000)

SH="class:SheetPresentationWindow"   # whichever SwiftUI sheet is up (the palette, when a scenario opened it by command)
def popen(): return [{"op":"command","id":"showCommandPalette"},{"op":"ext","id":"pause","text":"300"},idle]
def ptype2(t): return [{"op":"reading","id":"fieldText","window":SH,"text":t},idle]
def psubmit2(): return [{"op":"accounts","id":"click","window":SH,"text":"100,20"},{"op":"ext","id":"pause","text":"400"},idle]

# D (defect): commands that cannot act are offered enabled and silently do nothing
st=[loaded,idle]
for q in ["print","export","mute","vip","snooze","tag","send in one hour","correspondence","message source","move to inbox"]:
    st+=popen()+ptype2(q)+cap("palette-"+q.replace(" ","-"))+psubmit2()
    st+=expect("'%s' with no account, no message, no composer: must be unavailable (palette stays open, as it does for Reply)"%q,{"sheetAttached":True})
w("xk-defect-palette-enabled-noop",st,["S45 command palette","TASKS.md: commands have enabled/focus predicates"],t=120000)

# D2 (defect): Empty Trash asks to permanently erase a Trash that does not exist
st=[loaded,idle]+popen()+ptype2("empty trash")+psubmit2()+cap("empty-trash-with-no-account")
st+=expect("no account, no Trash: Empty Trash must not raise a permanent-erase confirmation",{"windowCount":1},M("accountCount","0"))
w("xk-defect-empty-trash-without-trash",st,["Empty Trash","command predicates"],t=60000)

# F: main window sizes, both layouts, empty and with mail
def sizes(tag):
    o=[]
    for lay in ["layoutWidescreen","layoutClassic"]:
        o+=[{"op":"command","id":lay},idle,{"op":"resizeWindow","window":"main","contentSize":"min"},idle]+cap("%s-%s-min"%(tag,lay),["main"])
        o+=expect("%s %s at min: fits a 1280x768 screen"%(tag,lay),window="main",rules=["nonBlank","fitsScreen"])
        o+=[{"op":"resizeWindow","window":"main","contentSize":[1240,700]},idle]+cap("%s-%s-large"%(tag,lay),["main"])
        o+=expect("%s %s large"%(tag,lay),M("reading:tableFound","true"),window="main",rules=["nonBlank"])
    return o
w("xk-defect-main-window-min-width",[loaded,idle]+cap("launch",["main"])+sizes("empty"),["main window minimum size","RootView: min 180 + 480 + 360 must fit any supported screen"],t=90000)
def sizes_mail():
    o=[]
    for lay,rules in [("layoutWidescreen",["nonBlank"]),("layoutClassic",["nonBlank","fitsScreen"])]:   # widescreen min: see xk-defect-main-window-min-width
        o+=[{"op":"command","id":lay},idle,{"op":"resizeWindow","window":"main","contentSize":"min"},idle]+cap("mail-%s-min"%lay,["main"])
        o+=expect("mail %s at min"%lay,M("reading:tableFound","true"),M("reading:selectedSubject","Invoice R0012345678 for August 2026"),window="main",rules=rules)
        o+=[{"op":"resizeWindow","window":"main","contentSize":[1900,1000]},idle]+cap("mail-%s-large"%lay,["main"])
        o+=expect("mail %s large"%lay,M("reading:tableFound","true"),M("reading:errorText","none"),window="main",rules=["nonBlank","onScreen"])
    return o
st=alice()+[{"op":"reading","id":"selectMessage","text":"Invoice R0012345678"},idle]+sizes_mail()
w("xk-main-window-sizes-mail",st,["main window min and large, both layouts, message open"],req=True)

# G: main window closed
pre=[loaded,idle,{"op":"openWindow","target":"status"},{"op":"wait","until":"window:status"},idle,
    {"op":"closeWindow","window":"main"},idle]+expect("main closed, Status still open",{"windowCount":1})
st=pre+[{"op":"command","id":"showContacts"},{"op":"ext","id":"pause","text":"800"},idle]
st+=expect("control: the first menu command after closing the main window still works (People > Contacts)",{"windowCount":2})
st+=[{"op":"command","id":"showDiagnostics"},{"op":"ext","id":"pause","text":"800"},idle]+cap("second-command-diagnostics")
st+=expect("the second menu command works too (Help > Diagnostics opens)",{"windowCount":3})
st+=[{"op":"command","id":"showProjects"},{"op":"ext","id":"pause","text":"800"},idle]
st+=expect("and the third (Projects > Projects Board opens)",{"windowCount":4})
st+=[{"op":"command","id":"showCommandPalette"},{"op":"ext","id":"pause","text":"1000"},idle]+cap("palette-requested-with-main-closed")
st+=expect("View > Command Palette shows the palette, bringing the main window back to host it",{"windowCount":6},{"sheetAttached":True})
w("xk-defect-main-closed-commands-dead",st,["close the main window and reopen it","menu commands keep working without the main window"],t=60000)

# H: keyboard shortcuts through the real menu key equivalents
def key(k): return [{"op":"ext","id":"keyEquivalent","window":"main","text":k},{"op":"ext","id":"pause","text":"400"},idle]
st=alice()+key("cmd+shift+k")+cap("palette-by-shortcut")+expect("shift-cmd-K opens the palette",{"sheetAttached":True},{"windowCount":2})
st+=ptype2("classic")+psubmit2()+expect("ran",M("reading:layout","classic"),{"sheetAttached":False})
# cmd-/ is not driven here: the synthetic key event was handled in 2 runs and "nothing handled" in 5 (see the xk report); the menu command is posted instead.
st+=[{"op":"command","id":"showShortcuts"},{"op":"ext","id":"pause","text":"500"},idle]+cap("cheat-sheet-with-mail")+expect("Help > Keyboard Shortcuts opens the cheat sheet",{"sheetAttached":True},{"windowCount":2})
st+=[{"op":"accounts","id":"click","window":SH,"text":"572,536"},{"op":"ext","id":"pause","text":"500"},idle]
st+=expect("Done closes the cheat sheet",{"sheetAttached":False},{"windowCount":1})
st+=key("cmd+n")+cap("compose-by-shortcut")+expect("cmd-N opens a composer",{"windowCount":2},M("ext:composeWindows","1"))
w("xk-keyboard-menu-shortcuts",st,["keyboard shortcuts","menu key equivalents","cheat sheet"],req=True,t=150000)

# E: destructive commands from the palette while the account is read-only (writes never enabled)
AL="class:_NSAlertPanel"
st=alice()+[{"op":"reading","id":"selectMessage","text":"Invoice R0012345678"},idle,{"op":"reading","id":"remember"}]
st+=popen()+cap("palette-with-selection")
for q in ["archive","delete","mark all"]:
    st+=popen()+ptype2(q)+cap("palette-"+q.replace(" ","-"))+psubmit2()+[{"op":"ext","id":"pause","text":"1200"},idle]+cap("after-"+q.replace(" ","-"))
    st+=[{"op":"ext","id":"waitExt","name":"pendingCount","text":"0"}]
    st+=[{"op":"assert","label":q+" from the palette on a read-only account queued nothing and left the message","expect":[M("ext:pendingCount","0"),M("reading:rememberedInList","true")]}]
w("xk-palette-destructive-readonly",st,["write safety","palette archive/delete/mark-all on a read-only account never queues a write"],req=True)

# L (defect): Empty Trash does not say whose Trash
st=[loaded,{"op":"addAccount","persona":"alice","as":"alice","waitFor":"synced","timeoutMs":90000},
    {"op":"addAccount","persona":"bob","as":"bob","waitFor":"synced","timeoutMs":90000},
    {"op":"reading","id":"selectMailbox","accountRef":"bob","text":"inbox"},idle,
    {"op":"command","id":"emptyTrash","allowServerWrites":True,"label":"opens the confirmation only; never confirmed, writes never enabled"},
    {"op":"ext","id":"pause","text":"800"},idle]+cap("empty-trash-two-accounts")
st+=[{"op":"accounts","id":"waitNode","window":AL,"find":{"text":"Cancel"},"value":True,"timeoutMs":3000,"label":"control: the confirmation is up"},
     {"op":"accounts","id":"waitNode","window":AL,"find":{"text":"bob@local.test"},"value":True,"timeoutMs":3000,
      "label":"the permanent-erase confirmation names the account whose Trash it will empty"}]
w("xk-defect-empty-trash-unnamed-account",st,["Empty Trash per account","irreversible action names its target"],req=True,t=150000)

# M: auxiliary windows with mail
st=alice()+[{"op":"reading","id":"selectMessage","text":"Invoice R0012345678"},idle]
for t in ["status","contacts","projects","diagnostics","outbox","aiActivity"]:
    st+=[{"op":"openWindow","target":t},{"op":"wait","until":"window:"+t},idle]+cap(t+"-mail",[t])
    st+=[{"op":"resizeWindow","window":t,"contentSize":"min"},idle]+cap(t+"-mail-min",[t])+expect(t,{"windowCount":2},window=t,rules=["nonBlank","fitsScreen","onScreen"])
    st+=[{"op":"closeWindow","window":t},idle]
st+=expect("all closed",{"windowCount":1},M("reading:selectedSubject","Invoice R0012345678 for August 2026"))
w("xk-aux-windows-mail",st,["aux windows with mail","min size","close"],req=True)

# N (defect): Message Source twice = two identical stacked windows; the window has no minimum size
st=alice()+[{"op":"reading","id":"selectMessage","text":"Invoice R0012345678"},idle]
st+=[{"op":"reading","id":"showSource"},{"op":"wait","until":"window:source","timeoutMs":20000},idle]+cap("source",["source"])
st+=expect("source window",{"windowCount":2},window="source",rules=["nonBlank","resizable","fitsScreen"])
st+=[{"op":"command","id":"viewSource"},{"op":"ext","id":"pause","text":"2500"},idle]+cap("source-twice")
st+=expect("Message Source twice for the same message re-fronts the one window (every other auxiliary window does)",{"windowCount":2})
st+=[{"op":"resizeWindow","window":"source","contentSize":"min","label":"the source window declares a minimum size"}]
w("xk-defect-message-source-duplicates",st,["message source opened twice","window minimum size"],req=True)

# S: settings twice, sizes; cheat sheet; a second sheet requested while one is up
st=[loaded,idle,{"op":"wait","until":"settingsOpenerReady"},{"op":"openWindow","target":"settings","tab":"general"},{"op":"wait","until":"window:settings"},idle]
st+=cap("settings-general",["settings"])+expect("settings",{"windowCount":2},window="settings",rules=["nonBlank","resizable","movable","fitsScreen"])
st+=[{"op":"reading","id":"settingsTab","tab":"general"},idle]+expect("Settings twice: one window",{"windowCount":2})
for tab in ["rules","tags","ai","accounts","licence","general"]:
    st+=[{"op":"reading","id":"settingsTab","tab":tab},idle,{"op":"resizeWindow","window":"settings","contentSize":"min"},idle]+cap("settings-%s-min"%tab,["settings"])
    st+=expect("settings %s at min"%tab,{"windowCount":2},window="settings",rules=["nonBlank","fitsScreen"])
st+=[{"op":"resizeWindow","window":"settings","contentSize":[1200,700]},idle]+cap("settings-general-large",["settings"])
st+=[{"op":"closeWindow","window":"settings"},idle]+expect("closed",{"windowCount":1})
st+=[{"op":"command","id":"showShortcuts"},{"op":"ext","id":"pause","text":"500"},idle]+cap("cheat-sheet")
st+=expect("cheat sheet",{"sheetAttached":True},window=SH,rules=["fitsScreen"])
st+=[{"op":"command","id":"showCommandPalette"},{"op":"ext","id":"pause","text":"500"},idle]+cap("palette-requested-over-cheat-sheet")
st+=[{"op":"accounts","id":"click","window":SH,"text":"572,536"},{"op":"ext","id":"pause","text":"1200"},idle]+cap("after-done")
w("xk-settings-and-sheets",st,["settings window twice and at min per tab","cheat sheet","second sheet requested while one is up"],t=120000)


