#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xe explorer seed for the Rules / Run Now journey (GreenMail on 127.0.0.1 only).

Usage:
  seed.py reset            wipe and re-seed xe-r1 / xe-r2 / xe-send @local.test (deterministic)
  seed.py dump [account]   print every folder's messages with flags (server truth after a run)

Accounts are auto-created by SMTP delivery; GreenMail gives them password == address.
Only addresses starting with "xe-" are ever touched.
"""
import imaplib, smtplib, sys, time, re
from email.message import EmailMessage

HOST, IMAP, SMTP = "127.0.0.1", 3143, 3025
R1, R2, SND = "xe-r1@local.test", "xe-r2@local.test", "xe-send@local.test"
FOLDERS = ["Archive", "Trash", "Sent", "Receipts"]


def msg(frm, to, subj, mid=None, body="xe seed body\n"):
    m = EmailMessage()
    m["From"], m["To"], m["Subject"] = frm, to, subj
    m["Date"] = "Mon, 05 Oct 2026 09:00:00 +0000"
    if mid:
        m["Message-ID"] = mid
    m.set_content(body)
    return m


def deliver(msgs):
    s = smtplib.SMTP(HOST, SMTP)
    for m in msgs:
        s.send_message(m)
    s.quit()


def login(acct):
    assert acct.startswith("xe-")
    i = imaplib.IMAP4(HOST, IMAP)
    i.login(acct, acct)
    return i


def folders(i):
    out = []
    for line in i.list()[1]:
        if line:
            out.append(line.decode().rsplit(' "." ', 1)[-1].strip('"'))
    return out


def wipe(acct):
    deliver([msg("xe-seed@local.test", acct, "xe bootstrap")])  # makes sure the account exists
    time.sleep(0.3)
    i = login(acct)
    for f in folders(i):
        i.select('"%s"' % f)
        i.store("1:*", "+FLAGS", "\\Deleted")
        i.expunge()
        i.close()
    # Folders are never deleted here: GreenMail dropped the connection on DELETE (seen 2026-10-05).
    have = set(folders(i))
    for f in FOLDERS:
        if f not in have:
            i.create('"%s"' % f)
    # "Filed" is SUBSCRIBED, so Castellum syncs it by default (unsubscribed custom folders and
    # Archive are listed but not synced: sync.rs should_sync_by_default).
    if "Filed" not in have:
        i.create('"Filed"')
    i.subscribe('"Filed"')
    return i


def reset():
    i = wipe(R1)
    long_subj = "XE Long " + "quarterly planning review and budget follow-up " * 6
    deliver(
        [msg("XE News <news@xe-news.test>", R1, "XE Weekly %d" % n) for n in (1, 2, 3)]
        + [msg("XE Shop Billing <billing@xe-shop.test>", R1, "XE Invoice %d" % n) for n in (1001, 1002)]
        + [msg("XE Boss <boss@xe-corp.test>", R1, long_subj.strip())]
        + [msg("XE List <list@xe-list.test>", R1, "XE Dup notice", mid="<xe-dup-1@xe-list.test>") for _ in (1, 2)]
    )
    t = imaplib.Time2Internaldate(time.time())
    i.append("Trash", "", t, msg("XE News <news@xe-news.test>", R1, "XE Weekly 0 (in Trash)").as_bytes())
    i.append("Receipts", "", t, msg("XE Shop Billing <billing@xe-shop.test>", R1, "XE Invoice 0999 (filed)").as_bytes())
    i.logout()
    i = wipe(R2)
    deliver(
        [msg("XE News <news@xe-news.test>", R2, "XE Weekly %s" % n) for n in ("A", "B")]
        + [msg("XE Shop Billing <billing@xe-shop.test>", R2, "XE Invoice 2001")]
    )
    i.logout()
    wipe(SND).logout()
    time.sleep(0.5)


def dump(accts):
    for a in accts:
        i = login(a)
        print("== %s" % a)
        for f in folders(i):
            typ, n = i.select('"%s"' % f, readonly=True)
            cnt = int(n[0])
            print("  [%s] %d" % (f, cnt))
            if cnt:
                typ, data = i.fetch("1:*", "(FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT)])")
                for d in data:
                    if isinstance(d, tuple):
                        flags = re.search(rb"FLAGS \(([^)]*)\)", d[0])
                        subj = d[1].decode(errors="replace").replace("\r\n", " ").strip()[:70]
                        print("     %-28s %s" % ((flags.group(1).decode() if flags else "?"), subj))
            i.close()
        i.logout()


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "dump"
    if cmd == "reset":
        reset()
        print("xe seed: reset done")
    else:
        dump(sys.argv[2:] or [R1, R2, SND])
