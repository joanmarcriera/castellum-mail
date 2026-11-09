#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
# xf explorer: run xf-defect-mark-all-read-replay-marks-new-mail and deliver the late message at the right moment.
# Usage: late-mail.sh <label>   (GreenMail 127.0.0.1 only; reseeds xf-t@local.test; log: build/explore/xf/<label>.log)
cd "$(dirname "$0")/../../.." || exit 2
D=build/explore/xf; S=xf-defect-mark-all-read-replay-marks-new-mail; L="$1"
python3 $D/seed.py t > $D/$L-seed.log 2>&1
before=$(ls -d build/ui-snapshots/$S/*/ 2>/dev/null | wc -l | tr -d ' ')
scripts/ui-check.sh --no-build $S > $D/$L.log 2>&1 &
pid=$!
for i in $(seq 1 240); do
  new=$(ls -d build/ui-snapshots/$S/*/01-marked-all-while-offline 2>/dev/null | tail -1)
  if [ -n "$new" ] && [ "$(find "$new" -newer $D/$L-seed.log -name main.png | wc -l | tr -d ' ')" != "0" ]; then break; fi
  sleep 0.5
done
python3 - <<'P'
import smtplib, time
from email.message import EmailMessage
from email.utils import formatdate
m = EmailMessage(); m["From"] = "XF Sender <xf-sender@local.test>"; m["To"] = "xf-t@local.test"
m["Subject"] = "XF t late arrival"; m["Date"] = formatdate(time.time(), localtime=True)
m["Message-ID"] = f"<xf-late-{time.time_ns()}@local.test>"; m.set_content("Arrived after Mark All as Read was pressed.\n")
with smtplib.SMTP("127.0.0.1", 3025) as s: s.send_message(m)
print("late mail delivered", time.strftime("%H:%M:%S"))
P
wait $pid; echo "exit $?" >> $D/$L.log
python3 $D/imapstate.py t >> $D/$L.log
