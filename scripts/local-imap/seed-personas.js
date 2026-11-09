#!/usr/bin/env node
/* SPDX-License-Identifier: Apache-2.0 */

/**
 * Seed the GreenMail persona accounts with a small, realistic set of mail.
 *
 * Usage:
 *   node scripts/local-imap/seed-personas.js            # one batch (~30 mails)
 *   node scripts/local-imap/seed-personas.js --dry-run  # print what would be sent
 *
 * Prerequisites:
 *   docker compose -f scripts/local-imap/docker-compose.yml up -d
 *   (the personas below must be in greenmail.users — see README.md)
 *
 * Why SMTP and not IMAP APPEND: GreenMail delivers a message to every local
 * recipient in To/Cc, so one send fills several mailboxes and threads show
 * up on both sides. Senders outside local.test are just From headers; the
 * SMTP session authenticates as a persona (GreenMail does not tie the
 * authenticated user to the From address).
 */

'use strict';

const nodemailer = require('nodemailer');

const SMTP_HOST = process.env.GREENMAIL_HOST || 'localhost';
const SMTP_PORT = Number(process.env.GREENMAIL_SMTP_PORT || 3025);
const DRY_RUN = process.argv.includes('--dry-run');

/** Fixed dev-only credentials; keep in sync with docker-compose.yml + README. */
const PERSONAS = {
  alice: { email: 'alice@local.test', pass: 'alice-pass', name: 'Alice Puig' },
  bob: { email: 'bob@local.test', pass: 'bob-pass', name: 'Bob Ferrer' },
  carol: { email: 'carol@local.test', pass: 'carol-pass', name: 'Carol Vidal' },
  dave: { email: 'dave@local.test', pass: 'dave-pass', name: 'Dave Soler' },
};

const hoursAgo = (h) => new Date(Date.now() - h * 3600 * 1000);
const addr = (p) => `"${p.name}" <${p.email}>`;

/**
 * Each entry: who authenticates (`as`), the headers, and an age in hours so
 * threads read in a sensible order. `inReplyTo` links replies into threads
 * by a fixed Message-ID per batch run.
 */
const batch = Date.now().toString(36);
const mid = (tag) => `<${tag}.${batch}@local.test>`;

const MESSAGES = [
  // --- alice <-> bob: a project thread (both mailboxes) ---
  {
    as: 'alice', from: addr(PERSONAS.alice), to: PERSONAS.bob.email, cc: PERSONAS.carol.email,
    subject: 'Sync window: 7 days or 30?', messageId: mid('sync-window'), age: 50,
    text: 'Bob, Carol —\n\nThe default sync window is 7 days. Support keeps asking for older mail on first launch. Do we go to 30 and eat the first-sync time?\n\nAlice',
  },
  {
    as: 'bob', from: addr(PERSONAS.bob), to: PERSONAS.alice.email, cc: PERSONAS.carol.email,
    subject: 'Re: Sync window: 7 days or 30?', messageId: mid('sync-window-1'), inReplyTo: mid('sync-window'), age: 46,
    text: '30 is fine on IMAP servers that support CONDSTORE; on the others it is a full fetch. I would keep 7 and add "load older" in the sidebar.\n\nBob',
  },
  {
    as: 'carol', from: addr(PERSONAS.carol), to: PERSONAS.alice.email, cc: PERSONAS.bob.email,
    subject: 'Re: Sync window: 7 days or 30?', messageId: mid('sync-window-2'), inReplyTo: mid('sync-window-1'), age: 40,
    text: 'Agree with Bob. Users forgive a button; they do not forgive a 20-minute first sync.\n\nCarol',
  },
  {
    as: 'alice', from: addr(PERSONAS.alice), to: PERSONAS.bob.email,
    subject: 'Release notes draft for 0.3.3', messageId: mid('relnotes'), age: 20,
    text: 'Attached inline below — shout if anything is wrong.\n\n- Fewer Keychain prompts at launch\n- Open Accounts… from the send error now works\n- Update check sentence on the site\n',
  },
  // --- external senders into alice: invoice, newsletter, recruiter ---
  {
    as: 'alice', from: '"Hetzner Billing" <billing@hetzner.example>', to: PERSONAS.alice.email,
    subject: 'Invoice R0012345678 for August 2026', messageId: mid('invoice'), age: 70,
    text: 'Dear customer,\n\nyour invoice for August 2026 is available. Amount: EUR 8.21. Due date: 25 September 2026.\n\nHetzner Online',
  },
  {
    as: 'alice', from: '"Rust Weekly" <news@rustweekly.example>', to: PERSONAS.alice.email,
    subject: 'This Week in Rust #612', messageId: mid('newsletter'), age: 90,
    html: '<h1>This Week in Rust</h1><p>Highlights: <b>rustls 0.24</b>, async closures stabilised, and a new IMAP parser crate worth a look.</p><p><a href="https://example.org/twir/612">Read online</a></p>',
    text: 'This Week in Rust #612 — rustls 0.24, async closures, a new IMAP parser crate.',
  },
  {
    as: 'alice', from: '"Priya at TalentBridge" <priya@talentbridge.example>', to: PERSONAS.alice.email,
    subject: 'Solutions Architect role — Cambridge, hybrid', messageId: mid('recruiter'), age: 12,
    text: 'Hi Alice,\n\nI came across your profile and think you would be a strong fit for a Solutions Architect position with a Cambridge scale-up. Would you have 15 minutes this week?\n\nBest,\nPriya',
  },
  // --- bob: receipts + notifications ---
  {
    as: 'bob', from: '"Amazon.co.uk" <auto-confirm@amazon.example>', to: PERSONAS.bob.email,
    subject: 'Your order #203-4471998-1122334 has been dispatched', messageId: mid('receipt-1'), age: 30,
    text: 'Hello Bob,\n\nYour order (USB-C hub, 1x) has been dispatched and will arrive Thursday.\n\nOrder total: GBP 24.99',
  },
  {
    as: 'bob', from: '"GitHub" <noreply@github.example>', to: PERSONAS.bob.email,
    subject: '[castellum] Run failed: rust-ci (main)', messageId: mid('ci-fail'), age: 8,
    text: 'rust-ci failed on main.\n\nJob: clippy\nError: unused variable `report` (deny(warnings))\n\nView the run: https://example.org/actions/runs/3861',
  },
  {
    as: 'bob', from: '"GitHub" <noreply@github.example>', to: PERSONAS.bob.email,
    subject: '[castellum] Run succeeded: rust-ci (main)', messageId: mid('ci-ok'), inReplyTo: mid('ci-fail'), age: 6,
    text: 'rust-ci passed on main after 3862.\n\nView the run: https://example.org/actions/runs/3862',
  },
  {
    as: 'bob', from: addr(PERSONAS.bob), to: PERSONAS.carol.email,
    subject: 'Contract for the Gist facet work', messageId: mid('contract'), age: 26,
    text: 'Carol, attached is the SOW for the rules/filters + Gist facet slice. Two weeks, fixed price, as discussed.\n\nBob',
  },
  // --- carol: client mail, a Catalan/Spanish thread, a calendar invite ---
  {
    as: 'carol', from: '"Montse Roca" <montse@estudi-roca.example>', to: PERSONAS.carol.email,
    subject: 'Pressupost web botiga — revisió', messageId: mid('cat-1'), age: 60,
    text: 'Hola Carol,\n\nGràcies pel pressupost. Podríem revisar l\'apartat de manteniment? Ens agradaria un pla trimestral en lloc de mensual.\n\nSalutacions,\nMontse',
  },
  {
    as: 'carol', from: addr(PERSONAS.carol), to: 'montse@estudi-roca.example', cc: PERSONAS.carol.email,
    subject: 'Re: Pressupost web botiga — revisió', messageId: mid('cat-2'), inReplyTo: mid('cat-1'), age: 55,
    text: 'Hola Montse,\n\nCap problema: pla trimestral a 240 EUR, mateix abast. Us envio la versió corregida demà.\n\nCarol',
  },
  {
    as: 'carol', from: '"Javier Ortega" <javier@ortega-abogados.example>', to: PERSONAS.carol.email,
    subject: 'Factura pendiente marzo — recordatorio', messageId: mid('es-1'), age: 33,
    text: 'Estimada Carol,\n\nle recordamos que la factura 2026-031 (450 EUR) sigue pendiente de pago. Puede abonarla por transferencia a la cuenta indicada en la factura.\n\nUn saludo,\nJavier Ortega',
  },
  {
    as: 'carol', from: addr(PERSONAS.alice), to: PERSONAS.carol.email, cc: PERSONAS.bob.email,
    subject: 'Invitation: Castellum 0.3.3 planning (Mon 15 Sep 10:00 BST)', messageId: mid('invite'), age: 14,
    text: 'Planning for 0.3.3: GUI pass feedback, S5 IMAP crate decision, rules/filters memo.\n\nJoin: https://meet.example.org/castellum-planning',
    icalendar: {
      method: 'REQUEST',
      content: [
        'BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//Castellum seed//EN', 'METHOD:REQUEST',
        'BEGIN:VEVENT', `UID:planning-${batch}@local.test`,
        'DTSTAMP:20260911T090000Z', 'DTSTART:20260915T090000Z', 'DTEND:20260915T100000Z',
        'SUMMARY:Castellum 0.3.3 planning', `ORGANIZER;CN=Alice Puig:mailto:${PERSONAS.alice.email}`,
        `ATTENDEE;CN=Carol Vidal;RSVP=TRUE:mailto:${PERSONAS.carol.email}`,
        `ATTENDEE;CN=Bob Ferrer;RSVP=TRUE:mailto:${PERSONAS.bob.email}`,
        'END:VEVENT', 'END:VCALENDAR',
      ].join('\r\n'),
    },
  },
  // --- unread noise for everyone (dave stays empty on purpose) ---
  {
    as: 'alice', from: '"Castellum Status" <status@castellummail.example>', to: [PERSONAS.alice.email, PERSONAS.bob.email, PERSONAS.carol.email].join(', '),
    subject: 'Scheduled maintenance: Sat 13 Sep 02:00–03:00 UTC', messageId: mid('status'), age: 3,
    text: 'The release feed (castellum-releases) will be read-only during the window. No action needed.',
  },
];

async function main() {
  const transports = new Map();
  const transportFor = (key) => {
    if (!transports.has(key)) {
      const p = PERSONAS[key];
      transports.set(key, nodemailer.createTransport({
        host: SMTP_HOST, port: SMTP_PORT, secure: false,
        auth: { user: p.email, pass: p.pass },
      }));
    }
    return transports.get(key);
  };

  let sent = 0;
  for (const m of MESSAGES) {
    const { as, age, ...mail } = m;
    mail.date = hoursAgo(age);
    if (DRY_RUN) {
      console.log(`[dry-run] as=${as} to=${mail.to} subject="${mail.subject}"`);
      continue;
    }
    try {
      await transportFor(as).sendMail(mail);
      sent += 1;
      console.log(`sent  ${String(mail.to).padEnd(48)} ${mail.subject}`);
    } catch (err) {
      console.error(`FAILED ${mail.to} "${mail.subject}": ${err.message}`);
      process.exitCode = 1;
    }
  }
  for (const t of transports.values()) t.close();
  if (!DRY_RUN) console.log(`\n${sent}/${MESSAGES.length} messages delivered to ${SMTP_HOST}:${SMTP_PORT}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
