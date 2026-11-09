#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
/**
 * Castellum - deliver ONE real calendar invitation (text/calendar METHOD:REQUEST) to carol.
 *
 * Usage:
 *   node scripts/local-imap/seed-invite.js          # one invite: Alice -> Carol (cc Bob)
 *   node scripts/local-imap/seed-invite.js --dry-run
 *
 * Why this exists: the "Invitation: Castellum 0.3.3 planning" mail in seed-personas.js passes its ICS
 * under an `icalendar` key, which nodemailer ignores (the key is `icalEvent`), so that seeded mail
 * has NO calendar part and the invite card (S55) never renders for it. This script sends the same
 * invite with the correct key. Subject carries "(with ICS)" so a UI scenario can tell it apart
 * (scripts/ui-scenarios/u3-s55-invite-card.json). Additive and harmless: one more mail in carol's
 * inbox (and a copy in bob's). Dev-only GreenMail credentials, same as seed-personas.js.
 */
'use strict';

const nodemailer = require('nodemailer');

const SMTP_HOST = process.env.GREENMAIL_HOST || 'localhost';
const SMTP_PORT = Number(process.env.GREENMAIL_SMTP_PORT || 3025);
const DRY_RUN = process.argv.includes('--dry-run');
const stamp = Date.now().toString(36);

const mail = {
  from: '"Alice Puig" <alice@local.test>',
  to: 'carol@local.test',
  cc: 'bob@local.test',
  subject: 'Invitation: Castellum 0.3.3 planning (with ICS)',
  messageId: `<invite-ics.${stamp}@local.test>`,
  date: new Date(Date.now() - 3600 * 1000),
  text: 'Planning for 0.3.3: GUI pass feedback, rules/filters memo.\n\nJoin: https://meet.example.org/castellum-planning',
  icalEvent: {
    method: 'REQUEST',
    content: [
      'BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Castellum seed//EN', 'METHOD:REQUEST',
      'BEGIN:VEVENT', `UID:planning-ics-${stamp}@local.test`,
      'DTSTAMP:20260911T090000Z', 'DTSTART:20261015T090000Z', 'DTEND:20261015T100000Z',
      'SUMMARY:Castellum 0.3.3 planning', 'ORGANIZER;CN=Alice Puig:mailto:alice@local.test',
      'ATTENDEE;CN=Carol Vidal;RSVP=TRUE:mailto:carol@local.test',
      'ATTENDEE;CN=Bob Ferrer;RSVP=TRUE:mailto:bob@local.test',
      'END:VEVENT', 'END:VCALENDAR',
    ].join('\r\n'),
  },
};

async function main() {
  if (DRY_RUN) { console.log(`[dry-run] ${mail.from} -> ${mail.to}: ${mail.subject}`); return; }
  const t = nodemailer.createTransport({
    host: SMTP_HOST, port: SMTP_PORT, secure: false, auth: { user: 'alice@local.test', pass: 'alice-pass' },
  });
  await t.sendMail(mail);
  t.close();
  console.log(`sent ${mail.subject}`);
}

main().catch((e) => { console.error(e); process.exit(1); });
