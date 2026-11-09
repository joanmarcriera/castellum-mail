# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Generates the xc-* scenarios (explorer journey: reply / reply-all / forward with attachments).
Usage: python3 build/explore/xc/mk_all.py   (fixtures: python3 build/explore/xc/seed.py)"""
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from gen import *
P = lambda ms: [{"op": "ext", "id": "pause", "text": str(ms)}]
W = "@compose"
def subj(t): return [{"op": "ext", "id": "setField", "window": W, "name": "subject", "text": t}]
def body(t, append=False): return [{"op": "ext", "id": "appendBody" if append else "setBody", "window": W, "text": t}]
def send(): return [{"op": "ext", "id": "keyEquivalent", "window": W, "text": "cmd+return"}]
def sent(): return [{"op": "ext", "id": "waitExt", "name": "composeWindows", "text": "0", "timeoutMs": 15000},
                    {"op": "ext", "id": "waitExt", "name": "pendingCount", "text": "0", "timeoutMs": 30000}, {"op": "wait", "until": "idle"}]
def wr(): return head(writes=True) + [{"op": "ext", "id": "setPref", "name": "undoSendDelay", "text": "0"}]
def other(email, ref, persona=False):
    a = {"op": "addAccount", "as": ref, "waitFor": "synced", "timeoutMs": 90000}
    a.update({"persona": email} if persona else {"email": email, "password": email})
    return [a, {"op": "reading", "id": "selectMailbox", "accountRef": ref, "text": "inbox"}, {"op": "wait", "until": "idle"},
            {"op": "reading", "id": "reSyncAll"}] + P(5000) + [{"op": "wait", "until": "idle"}]
alert = "class:_NSAlertPanel"
def press(label): return [{"op": "press", "window": alert, "find": {"label": label}}] + P(800)

# ---------- passing coverage ----------
st = head()
for s, cmd, nm, exp in [
    ("XC plain note", "reply", "plain", [("ext:subject", "Re: XC plain note"), ("ext:bodyPrefix:\n\nOn ", "true"), ("ext:bodyHas:Anna Puig wrote:\n> Hi,", "true"), ("ext:bodyHas:> Can you confirm the meeting on Friday?", "true"), ("ext:bodyCount:> ", "5")]),
    ("RE: XC upper prefix", "reply", "upper-re", [("ext:subject", "RE: XC upper prefix")]),
    ("Re: Re: XC doubled", "reply", "doubled-re", [("ext:subject", "Re: Re: XC doubled prefix")]),
    ("Fwd: XC already forwarded", "reply", "re-fwd", [("ext:subject", "Re: Fwd: XC already forwarded")]),
    ("XC pressupost amb adjunt", "reply", "catalan", [("ext:subject", "Re: XC pressupost amb adjunt"), ("ext:bodyHas:> T'adjunto el pressupost revisat. Ja em diràs què et sembla.", "true"), ("ext:bodyHas:> Salutacions,", "true")]),
    ("XC factura con adjunto", "replyAll", "spanish", [("ext:subject", "Re: XC factura con adjunto"), ("ext:bodyHas:Luis Gómez wrote:", "true"), ("ext:bodyHas:> Adjunto la factura de marzo.", "true")]),
    ("XC-NOSUBJECT", None, None, None),
]:
    if cmd is None: continue
    st += sel(s) + reply(cmd) + cap(nm) + expect(*exp, rules=["fitsScreen", "noZeroSizeTextFields"], window="title:" + exp[0][1]) + close()
write("xc-reply-prefill", ["reply/reply-all prefill: Re: prefix never doubled, RE: kept, quote + attribution for plain, Catalan and Spanish mail"], st, timeout=240000)

st = head() + sel("XC plain note") + reply("reply") + body("XC-NOWRITE-{run} see you Friday") + P(12000) + cap("typed") + send() + P(3000) + cap("send-refused")
st += expect(("ext:composeWindows", "1"), ("ext:pendingCount", "0"), ("ext:bodyHas:XC-NOWRITE", "true"))
write("xc-readonly-reply-no-write", ["write safety: reply typed + Send on a read-only (evaluation mode) account queues nothing and keeps the draft open"], st)

st = wr() + sel("XC plain note") + reply("reply") + subj("Re: XC plain note warn {run}")
for nm, txt in [("en", "Please find attached the agenda."), ("es", "Hola, adjunto la factura de marzo."), ("ca", "Bon dia, t'adjunto el pressupost revisat.")]:
    st += body(txt) + send() + P(1200) + cap("warn-" + nm)
    st += press("Cancel") + [{"op": "assert", "expect": [{"windowCount": 2}, {"model": "ext:composeWindows", "equals": "1"}]}]
st += body("Thanks, that works.\n\nOn 5 Oct 2026 at 13:48, Anna Puig wrote:\n> Please see the attached file for details.\n> Adjunto la factura.") + send() + sent()
st += expect(("ext:composeWindows", "0"))
st += sel("XC plain note") + reply("reply") + subj("Re: XC plain note anyway {run}") + body("See attached. {run}") + send() + P(1200) + press("Send Anyway") + sent() + cap("after-send-anyway", ["main"])
write("xc-forgotten-attachment-warning", ["HP5a forgotten-attachment warning: en/es/ca keyword, Cancel keeps the draft, quoted keyword ignored, Send Anyway sends"], st, timeout=240000)

st = wr() + sel("XC plain note") + reply("replyAll") + cap("replyall-compose") + subj("Re: XC plain note {run}")
st += body("Confirmed for Friday. {run}\n\nOn 5 Oct 2026 at 13:48, Anna Puig wrote:\n> Can you confirm the meeting on Friday?") + send() + sent()
st += other("xc-carl@local.test", "carl") + [{"op": "ext", "id": "selectMessage", "text": "Re: XC plain note {run}"}, {"op": "wait", "until": "idle"}] + [{"op": "ext", "id": "loadSource"}] + cap("carl-received", ["main"])
st += expect(("ext:messageCount:Re: XC plain note {run}", "1"), ("ext:src.has:In-Reply-To: ", "true"), ("ext:src.has:References: ", "true"),
             ("ext:src.has:To: \"Anna Puig\" <xc-anna@local.test>, \"Carl Roca\" <xc-carl@local.test>", "true"), ("ext:src.has:From: xc-me@local.test", "true"), ("ext:src.has:Cc:", "false"),
             ("reading:detailTextHas:> Can you confirm the meeting on Friday?", "true"))
write("xc-replyall-send-roundtrip", ["reply-all end to end on GreenMail: Cc'd recipient receives it, own address not a recipient (only From), In-Reply-To/References set, quote delivered"], st, timeout=240000)

st = head() + sel("XC twelve attachments") + P(1500) + cap("twelve") + expect(("reading:attachmentCount", "12"), rules=["nonBlank"], window="main")
st += sel("XC pressupost amb adjunt") + P(1500) + cap("two") + expect(("reading:attachmentCount", "2"), rules=["nonBlank"], window="main")
st += sel("XC plain note") + P(800) + cap("none") + expect(("reading:attachmentCount", "0"))
write("xc-attachment-strip-counts", ["reader attachment strip: 12 attachments, 2 attachments (pdf + png with a space in the name), minimum window, none"], st)

st = head() + sel("XC long quote") + reply("reply") + cap("long")
st += expect(("ext:bodyHas:> Line 400 of a very long message", "true"), ("ext:bodyCount:\n> Line ", "400"), rules=["fitsScreen", "noZeroSizeTextFields", "controlsReachable"], window="title:Re: XC long quote")
st += close() + sel("XC long quote") + reply("reply") + reply("replyAll") + cap("twice") + expect(("ext:composeWindows", "1"))
write("xc-reply-long-quote", ["reply to a 400-line message: full quote kept (never truncated), controls reachable at the default size, reply pressed twice"], st)

# ---------- defects (each FAILS on the current build) ----------
st = wr() + sel("XC replyto list") + reply("reply") + cap("reply-compose") + subj("Re: XC replyto list {run}") + body("Replying, as asked, to the list. {run}") + send() + sent()
st += other("xc-list@local.test", "list") + cap("list-inbox", ["main"])
st += expect(("ext:messageCount:Re: XC replyto list {run}", "1"))
write("xc-defect-reply-ignores-reply-to", ["DEFECT: Reply must go to the Reply-To address (xc-list@), not the From address"], st, timeout=240000)

st = head() + sel("XC html only") + reply("reply") + cap("html-quote") + expect(("ext:bodyHas:> Regards,", "true"), ("ext:bodyHas:numbersRevenue", "false"))
write("xc-defect-html-reply-quote-merges-blocks", ["DEFECT: the quote of an HTML-only mail must keep block boundaries (heading and paragraph on separate lines)"], st)

st = wr() + sel("XC Attachment policy question") + reply("reply") + body("We have none, as far as I know.") + send() + P(2500) + cap("after-send")
st += expect(("ext:composeWindows", "0")) + [{"op": "assert", "window": "main", "rules": ["noErrorAlert"]}]
write("xc-defect-forgotten-attachment-subject-false-positive", ["DEFECT: a reply whose only 'attachment' word is in the inherited Re: subject must send without the forgotten-attachment warning"], st)

st = wr() + sel("XC plain note") + reply("reply") + subj("Re: XC plain note bottom {run}") + body("\n\nYes, Friday works. Please find attached the agenda. {run}", append=True) + cap("bottom-posted") + send() + P(2500) + cap("after-send")
st += press("Cancel") + expect(("ext:composeWindows", "1"))
write("xc-defect-forgotten-attachment-bottom-post-missed", ["DEFECT: text the user types below the quoted original must still be scanned by the forgotten-attachment check"], st)

st = wr() + sel("XC pressupost amb adjunt") + reply("reply") + subj("Re: XC pressupost {run}") + body("Bon dia, us adjuntem el contracte signat. {run}") + send() + P(2500) + cap("ca-adjuntem")
st += press("Cancel") + expect(("ext:composeWindows", "1"))
write("xc-defect-forgotten-attachment-ca-verb-missed", ["DEFECT: Catalan 'adjuntem' (we attach) with nothing attached must raise the forgotten-attachment warning"], st)

st = wr() + sel("XC from myself") + reply("reply") + cap("reply-to-own") + subj("Re: XC from myself {run}") + body("Following up on my own note. {run}") + send() + sent()
st += other("bob", "bob", persona=True) + cap("bob-inbox", ["main"]) + expect(("ext:messageCount:Re: XC from myself {run}", "1"))
write("xc-defect-reply-to-own-message-goes-to-self", ["DEFECT: Reply on a message I sent must address its original recipients (Bob), not myself"], st, timeout=240000)

st = head() + sel("XC plain note") + P(800) + cap("baseline") + expect(("reading:windowMinHeightCompact", "true"))
st += sel("XC twelve attachments") + P(1500) + cap("twelve-attachments") + expect(("reading:attachmentCount", "12"), ("reading:windowMinHeightCompact", "true"), rules=["fitsScreen"], window="main")
write("xc-defect-attachment-strip-inflates-window", ["DEFECT: the reader attachment strip must scroll or collapse; 12 attachments must not raise the main window's minimum height above the screen"], st, screen=(1366, 768))

st = wr() + sel("XC plain note") + reply("reply") + subj("Re: XC plain note irt {run}") + body("Threading check. {run}") + send() + sent()
st += other("xc-anna@local.test", "anna") + [{"op": "ext", "id": "selectMessage", "text": "Re: XC plain note irt {run}"}, {"op": "wait", "until": "idle"}, {"op": "ext", "id": "loadSource"}] + cap("anna-received", ["main"])
st += expect(("ext:src.has:In-Reply-To: ", "true"), ("ext:src.has:In-Reply-To: <", "true"), ("ext:src.has:References: <", "true"))
write("xc-defect-reply-in-reply-to-without-angle-brackets", ["DEFECT: a sent reply's In-Reply-To and References must be RFC 5322 msg-ids in angle brackets, or other clients cannot thread it"], st, timeout=240000)

# ---------- passing: cancel half-way, reopen, send ----------
st = wr() + [{"op": "reading", "id": "clearMessage"}, {"op": "command", "id": "reply"}] + P(800) + expect(("ext:composeWindows", "0")) + [{"op": "assert", "expect": [{"windowCount": 1}]}]
st += sel("XC plain note") + reply("reply") + subj("Re: XC plain note draft {run}") + body("Half-written reply {run}\n\nOn 5 Oct 2026 at 13:48, Anna Puig wrote:\n> Hi,") + close()
st += [{"op": "ext", "id": "openFirstDraft", "email": ME}, {"op": "ext", "id": "waitExt", "name": "composeWindows", "text": "1"}, {"op": "wait", "until": "idle"}] + cap("reopened")
st += expect(("ext:bodyHas:Half-written reply", "true"), ("ext:bodyHas:> Hi,", "true")) + send() + sent()
st += other("xc-anna@local.test", "anna") + [{"op": "ext", "id": "selectMessage", "text": "Re: XC plain note draft {run}"}, {"op": "wait", "until": "idle"}, {"op": "ext", "id": "loadSource"}] + cap("anna-received", ["main"])
st += expect(("ext:messageCount:Re: XC plain note draft {run}", "1"), ("ext:src.has:In-Reply-To: ", "true"), ("ext:src.has:> Hi,", "true"))
write("xc-reply-draft-reopen-send", ["Reply with nothing selected opens nothing; a reply closed half-way is saved, reopens with its text and quote, and still carries In-Reply-To when sent"], st, timeout=240000)
