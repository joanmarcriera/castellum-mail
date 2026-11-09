# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# Exploratory + coverage scenarios, round 3 (run from the worktree root).
import json,sys
sys.path.insert(0,"build/explore/xk")
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
def alice(): return [loaded,{"op":"addAccount","persona":"alice","as":"alice","waitFor":"synced","timeoutMs":90000},
    {"op":"reading","id":"selectMailbox","accountRef":"alice","text":"inbox"},idle]
def popen(): return [{"op":"command","id":"showCommandPalette"},pause(300),idle]
def ptype(t): return [{"op":"reading","id":"fieldText","window":SH,"text":t},idle]
def psubmit(): return [{"op":"accounts","id":"click","window":SH,"text":"100,20"},pause(500),idle]
def key(k,win="main"): return [{"op":"ext","id":"keyEquivalent","window":win,"text":k},pause(500),idle]
def w(name,steps,covers,req=True,t=180000):
    d={"schema":1,"name":name,"covers":covers,"screen":S,"timeoutMs":t,"steps":steps+[{"op":"quit"}]}
    if req: d["requires"]={"greenmail":True,"personas":True}
    json.dump(d,open(f"scripts/ui-scenarios/{name}.json","w"),indent=1)
sel=[{"op":"reading","id":"selectMessage","text":"Invoice R0012345678"},idle]
which=sys.argv[1] if len(sys.argv)>1 else "all"
if which in("x2","all"):
    st=alice()+sel
    st+=[{"op":"command","id":"showCorrespondence"},pause(600),idle]+cap("correspondence-widescreen",["main"])
    st+=[{"op":"command","id":"layoutClassic"},idle]+cap("correspondence-classic",["main"])
    st+=[{"op":"resizeWindow","window":"main","contentSize":"min"},idle]+cap("correspondence-classic-min",["main"])
    st+=[{"op":"resizeWindow","window":"main","contentSize":[1200,700]},idle,{"op":"reading","id":"closeCorrespondence"},idle]
    st+=popen()+ptype("go to trash")+cap("palette-go-to-trash")+psubmit()+cap("after-go-to-trash",["main"])
    st+=popen()+ptype("fit columns")+psubmit()+cap("after-fit-columns",["main"])
    st+=popen()+ptype("go to inbox")+psubmit()+cap("after-go-to-inbox",["main"])
    st+=key("cmd+n")+cap("compose-open")
    st+=popen()+cap("palette-with-composer-open")+ptype("send in one hour")+psubmit()+cap("after-send-in-one-hour-empty-composer")
    st+=expect("nothing queued",M("ext:pendingCount","0"))
    st+=popen()+ptype("send")+cap("palette-send")+psubmit()+cap("after-send-empty-composer")
    st+=expect("nothing queued",M("ext:pendingCount","0"))
    w("xk-zz-explore2",st,["explore"])
