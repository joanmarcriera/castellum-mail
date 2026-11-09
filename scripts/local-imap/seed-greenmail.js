#!/usr/bin/env node
/* SPDX-License-Identifier: Apache-2.0 */

/**
 * GreenMail bulk seed script
 *
 * Usage:
 *   node scripts/local-imap/seed-greenmail.js --count 1000
 *   node scripts/local-imap/seed-greenmail.js --count 10000 --threads 500
 *
 * Prerequisites:
 *   docker compose -f scripts/local-imap/docker-compose.yml up -d
 */

'use strict';

const Imap = require('imap');

// Env overrides let a perf run target a dedicated GreenMail container/user
// (project-docs/perf/2026-09-sync-baseline.md) without disturbing the shared
// e2e instance on 3143. Defaults are unchanged.
const SMTP_USER = process.env.GREENMAIL_USER || 'test@local.test';
const SMTP_PASS = process.env.GREENMAIL_PASS || 'secret';
const IMAP_HOST = process.env.GREENMAIL_HOST || 'localhost';
const IMAP_PORT = parseInt(process.env.GREENMAIL_IMAP_PORT || '3143', 10);

const args = process.argv.slice(2);
const getArg = (name, fallback) => {
  const idx = args.indexOf(name);
  if (idx === -1) return fallback;
  const next = args[idx + 1];
  if (!next || next.startsWith('--')) return fallback;
  return next;
};

const TOTAL = Math.max(1, parseInt(getArg('--count', '1000'), 10));
const THREADS = Math.max(1, parseInt(getArg('--threads', String(Math.min(200, Math.max(10, Math.floor(TOTAL / 10))))), 10));
const SINCE_DAYS = Math.max(1, parseInt(getArg('--since-days', '7'), 10));
const INCLUDE_OLDER = args.includes('--include-older');

const FOLDERS = [
  'INBOX',
  'Archive',
  'Archive_2025',
  'Personal',
  'Receipts',
  'Projects',
  'Newsletters',
];

function connectImap() {
  return new Promise((resolve, reject) => {
    const imap = new Imap({
      host: IMAP_HOST,
      port: IMAP_PORT,
      tls: false,
      user: SMTP_USER,
      password: SMTP_PASS,
    });
    imap.once('ready', () => resolve(imap));
    imap.once('error', reject);
    imap.connect();
  });
}

function addBox(imap, name) {
  return new Promise((resolve) => {
    if (name === 'INBOX') return resolve();
    imap.addBox(name, () => resolve());
  });
}

function appendMessage(imap, mailbox, raw, flags) {
  return new Promise((resolve, reject) => {
    // Avoid passing options.date to node-imap (util.isDate incompatibility on newer Node).
    imap.append(raw, { mailbox, flags }, (err) => {
      if (err) reject(err);
      else resolve();
    });
  });
}

function buildRawEmail({
  from,
  to,
  cc,
  subject,
  date,
  messageId,
  inReplyTo,
  references,
  body,
}) {
  const headers = [];
  headers.push(`From: ${from}`);
  headers.push(`To: ${to}`);
  if (cc) headers.push(`Cc: ${cc}`);
  headers.push(`Subject: ${subject}`);
  headers.push(`Date: ${date.toUTCString()}`);
  headers.push(`Message-ID: ${messageId}`);
  if (inReplyTo) headers.push(`In-Reply-To: ${inReplyTo}`);
  if (references && references.length > 0) headers.push(`References: ${references.join(' ')}`);
  headers.push('MIME-Version: 1.0');
  headers.push('Content-Type: text/plain; charset=utf-8');
  headers.push('Content-Transfer-Encoding: 7bit');
  return `${headers.join('\r\n')}\r\n\r\n${body}\r\n`;
}

function buildRawHtmlEmail({ from, to, subject, date, messageId, htmlBody, textBody }) {
  const boundary = 'castellum-seed-boundary';
  return [
    `From: ${from}`,
    `To: ${to}`,
    `Subject: ${subject}`,
    `Date: ${date.toUTCString()}`,
    `Message-ID: ${messageId}`,
    'MIME-Version: 1.0',
    `Content-Type: multipart/alternative; boundary="${boundary}"`,
    '',
    `--${boundary}`,
    'Content-Type: text/plain; charset=utf-8',
    '',
    textBody,
    `--${boundary}`,
    'Content-Type: text/html; charset=utf-8',
    '',
    htmlBody,
    `--${boundary}--`,
    '',
  ].join('\r\n');
}

const HTML_SAMPLES = [
  {
    subject: 'HTML sample: newsletter',
    htmlBody:
      '<table width="600" cellpadding="0" cellspacing="0" align="center"><tr><td bgcolor="#f6f6f6" style="padding:24px;font-family:sans-serif">' +
      '<h1 style="color:#1F7A8C">Castellum Weekly</h1><p>Hello <strong>reader</strong>, this is a <a href="https://example.com">table-soup newsletter</a>.</p>' +
      '<img src="https://example.com/hero.png" width="560" alt="hero image"></td></tr></table>',
    textBody: 'Castellum Weekly - table-soup newsletter (text fallback).',
  },
  {
    subject: 'HTML sample: html-only inline image',
    htmlBody:
      '<div style="font-family:serif"><p>Inline pixel: <img src="data:image/gif;base64,R0lGODlhAQABAAAAACw=" alt="dot"></p>' +
      '<p>Embedded ref: <img src="cid:attached-part-1" alt="cid placeholder"></p></div>',
    textBody: 'HTML-only message with inline + cid images (text fallback).',
  },
  {
    subject: 'HTML sample: hostile',
    htmlBody:
      '<p>Before</p><script>document.title="pwned"</script>' +
      '<img src="https://tracker.example/p.gif" onerror="alert(1)">' +
      '<a href="javascript:alert(2)">click</a><iframe src="https://evil.example"></iframe>' +
      '<style>body{background:url(https://tracker.example/css.gif)}</style><p>After</p>',
    textBody: 'Hostile HTML sample (text fallback).',
  },
];

async function seedHtmlSamples(imap) {
  for (let i = 0; i < HTML_SAMPLES.length; i += 1) {
    const s = HTML_SAMPLES[i];
    const raw = buildRawHtmlEmail({
      from: '"HTML Seeder" <html-seed@local.test>',
      to: SMTP_USER,
      subject: s.subject,
      date: new Date(),
      messageId: `<seed-html-${i}@local.test>`,
      htmlBody: s.htmlBody,
      textBody: s.textBody,
    });
    await appendMessage(imap, 'INBOX', raw, []);
  }
  console.log(`  seeded ${HTML_SAMPLES.length} HTML samples`);
}

// --- Attachment / inline-image samples ---------------------
//
// Generates a tiny PDF and a tiny PNG in-process (no binary files committed)
// and seeds:
//   1. a message with both as real, non-inline attachments (Content-Disposition:
//      attachment), proving has_attachments=true / AttachmentDescriptor parsing.
//   2. a multipart/related HTML message whose body references the PNG via a
//      `cid:` URI, proving inline cid: image resolution.

/**
 * Builds a small-but-structurally-valid single-page PDF containing `text`.
 * Byte offsets in the xref table are computed from the objects actually
 * emitted, so this stays correct if the text or objects change.
 */
function buildMinimalPdf(text) {
  const escaped = String(text).replace(/([()\\])/g, '\\$1');
  const stream = `BT /F1 18 Tf 20 100 Td (${escaped}) Tj ET`;
  const objs = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>',
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
    `<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`,
  ];

  let body = '%PDF-1.4\n';
  const offsets = [0]; // object 0 is the free-list head, unused below
  objs.forEach((obj, i) => {
    offsets.push(body.length);
    body += `${i + 1} 0 obj\n${obj}\nendobj\n`;
  });

  const xrefStart = body.length;
  let xref = `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n`;
  for (let i = 1; i <= objs.length; i += 1) {
    xref += `${String(offsets[i]).padStart(10, '0')} 00000 n \n`;
  }
  body += xref;
  body += `trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${xrefStart}\n%%EOF`;
  return Buffer.from(body, 'latin1');
}

// Minimal 1x1 red PNG (valid image, 68 bytes decoded) - same fixture used by
// test-inline-image-cid.js.
const SEED_PNG_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8/5+hHgAHggJ/PchI7wAAAABJRU5ErkJggg==';

/** Folds base64 to 76-char lines per RFC 2045. */
function foldBase64(buffer) {
  const b64 = buffer.toString('base64');
  const lines = [];
  for (let i = 0; i < b64.length; i += 76) lines.push(b64.slice(i, i + 76));
  return lines.join('\r\n');
}

function buildRawAttachmentEmail({ from, to, subject, date, messageId, textBody, pdfBuffer, pngBuffer }) {
  const boundary = 'castellum-seed-attach';
  return [
    `From: ${from}`,
    `To: ${to}`,
    `Subject: ${subject}`,
    `Date: ${date.toUTCString()}`,
    `Message-ID: ${messageId}`,
    'MIME-Version: 1.0',
    `Content-Type: multipart/mixed; boundary="${boundary}"`,
    '',
    `--${boundary}`,
    'Content-Type: text/plain; charset=utf-8',
    '',
    textBody,
    `--${boundary}`,
    'Content-Type: application/pdf',
    'Content-Transfer-Encoding: base64',
    'Content-Disposition: attachment; filename="castellum-seed-report.pdf"',
    '',
    foldBase64(pdfBuffer),
    `--${boundary}`,
    'Content-Type: image/png',
    'Content-Transfer-Encoding: base64',
    'Content-Disposition: attachment; filename="castellum-seed-photo.png"',
    '',
    foldBase64(pngBuffer),
    `--${boundary}--`,
    '',
  ].join('\r\n');
}

function buildRawInlineImageEmail({ from, to, subject, date, messageId, textBody, htmlBody, cid, pngBuffer }) {
  const relatedBoundary = 'castellum-seed-related';
  const altBoundary = 'castellum-seed-alt';
  return [
    `From: ${from}`,
    `To: ${to}`,
    `Subject: ${subject}`,
    `Date: ${date.toUTCString()}`,
    `Message-ID: ${messageId}`,
    'MIME-Version: 1.0',
    `Content-Type: multipart/related; boundary="${relatedBoundary}"`,
    '',
    `--${relatedBoundary}`,
    `Content-Type: multipart/alternative; boundary="${altBoundary}"`,
    '',
    `--${altBoundary}`,
    'Content-Type: text/plain; charset=utf-8',
    '',
    textBody,
    `--${altBoundary}`,
    'Content-Type: text/html; charset=utf-8',
    '',
    htmlBody,
    `--${altBoundary}--`,
    `--${relatedBoundary}`,
    'Content-Type: image/png',
    'Content-Transfer-Encoding: base64',
    `Content-ID: <${cid}>`,
    'Content-Disposition: inline; filename="castellum-seed-inline.png"',
    '',
    foldBase64(pngBuffer),
    `--${relatedBoundary}--`,
    '',
  ].join('\r\n');
}

async function seedAttachmentSamples(imap) {
  const pdfBuffer = buildMinimalPdf('Castellum seed report');
  const pngBuffer = Buffer.from(SEED_PNG_BASE64, 'base64');
  const cid = `castellum-seed-inline-${Date.now().toString(36)}`;

  const attachmentRaw = buildRawAttachmentEmail({
    from: '"Attachment Seeder" <attachment-seed@local.test>',
    to: SMTP_USER,
    subject: 'Attachment sample: PDF report + PNG photo',
    date: new Date(),
    messageId: `<seed-attachment-${Date.now().toString(36)}@local.test>`,
    textBody: 'Two attachments: a generated PDF report and a generated PNG photo.',
    pdfBuffer,
    pngBuffer,
  });
  await appendMessage(imap, 'INBOX', attachmentRaw, []);

  const inlineRaw = buildRawInlineImageEmail({
    from: '"Inline Image Seeder" <inline-image-seed@local.test>',
    to: SMTP_USER,
    subject: 'Inline image sample: HTML with cid: image',
    date: new Date(),
    messageId: `<seed-inline-image-${Date.now().toString(36)}@local.test>`,
    textBody: 'HTML body with an inline cid: image (text fallback).',
    htmlBody: `<div style="font-family:sans-serif"><p>Inline photo below, referenced via cid:</p><img src="cid:${cid}" alt="inline seed photo"></div>`,
    cid,
    pngBuffer,
  });
  await appendMessage(imap, 'INBOX', inlineRaw, []);

  console.log('  seeded 1 attachment sample (PDF + PNG) and 1 inline cid: image sample');
}

// Deterministic multi-recipient project threads for the contacts e2e
// (native/crates/castellum-core/tests/contacts_greenmail.rs): alice/bob are
// cross-cc'd on most messages, carol appears once same-message with alice and
// otherwise only shares the thread — so weight(alice,bob) > weight(alice,carol)
// and the thread-only bonus is exercised. References headers are consistent
// so the thread projection links every reply.
const PROJECT_PEOPLE = {
  alice: '"Alice Novak" <alice@example.test>',
  bob: '"Bob Iyer" <bob@example.test>',
  carol: '"Carol Reyes" <carol@example.test>',
};

const PROJECT_THREADS = [
  {
    key: 'p1',
    subject: 'Project Phoenix kickoff',
    messages: [
      { from: 'alice', cc: ['bob', 'carol'] },
      { from: 'bob', cc: ['alice'] },
      { from: 'alice', cc: ['bob'] },
      { from: 'bob', cc: ['alice'] },
    ],
  },
  {
    key: 'p2',
    subject: 'Project Phoenix budget',
    messages: [
      { from: 'alice', cc: ['bob'] },
      { from: 'carol', cc: [] }, // thread-only co-occurrence with alice/bob
      { from: 'bob', cc: ['alice'] },
    ],
  },
];

async function seedProjectThreads(imap) {
  let count = 0;
  // Per-run tag so re-seeding appends fresh threads instead of duplicate
  // Message-IDs (mirrors the Rust e2e's unique-tag approach).
  const runTag = Date.now().toString(36);
  for (const thread of PROJECT_THREADS) {
    const rootId = `<project-${thread.key}-${runTag}-0@example.test>`;
    const references = [];
    let previousId = rootId;
    for (let i = 0; i < thread.messages.length; i += 1) {
      const m = thread.messages[i];
      const messageId = `<project-${thread.key}-${runTag}-${i}@example.test>`;
      // Recent + strictly ordered so decay is ~1 and ordering is stable.
      const date = new Date(Date.now() - (thread.messages.length - i) * 60 * 60 * 1000);
      const raw = buildRawEmail({
        from: PROJECT_PEOPLE[m.from],
        to: SMTP_USER,
        cc: m.cc.map((k) => PROJECT_PEOPLE[k]).join(', ') || undefined,
        subject: i === 0 ? thread.subject : `Re: ${thread.subject}`,
        date,
        messageId,
        inReplyTo: i === 0 ? null : previousId,
        references: i === 0 ? [] : references.slice(),
        body: `Message ${i + 1} of ${thread.subject}.`,
      });
      await appendMessage(imap, 'INBOX', raw, ['\\Seen']);
      references.push(messageId);
      previousId = messageId;
      count += 1;
    }
  }
  console.log(`  seeded ${count} PROJECT_THREADS messages (contacts e2e)`);
}

function randomInt(max) {
  return Math.floor(Math.random() * max);
}

function pickFolder(i, threadIndex) {
  const roll = (i + threadIndex) % 10;
  if (roll < 6) return 'INBOX';
  return FOLDERS[(i + threadIndex) % FOLDERS.length];
}

function buildSubject(threadIndex) {
  const topics = [
    'Weekly status',
    'Invoice follow-up',
    'Meeting notes',
    'Project Alpha',
    'Travel changes',
    'Budget review',
    'Support ticket',
    'Draft proposal',
    'Bug report',
    'Onboarding task',
  ];
  return `[Thread ${threadIndex + 1}] ${topics[threadIndex % topics.length]}`;
}

async function main() {
  console.log(`=== GreenMail Bulk Seed ===`);
  console.log(`Target: ${TOTAL} emails in ${THREADS} threads`);
  console.log(`Date window: last ${SINCE_DAYS} days${INCLUDE_OLDER ? ' (+ 30% older)' : ''}`);

  const imap = await connectImap();
  for (const folder of FOLDERS) {
    await addBox(imap, folder);
  }

  let created = 0;
  const perThread = Math.floor(TOTAL / THREADS);
  const remainder = TOTAL % THREADS;

  for (let t = 0; t < THREADS; t += 1) {
    const threadCount = perThread + (t < remainder ? 1 : 0);
    const subject = buildSubject(t);
    const rootMessageId = `<seed-${t}-0@local.test>`;
    const references = [rootMessageId];
    let previousMessageId = rootMessageId;

    for (let i = 0; i < threadCount; i += 1) {
      const messageId = `<seed-${t}-${i}@local.test>`;
      const inReplyTo = i === 0 ? null : previousMessageId;
      const refs = i === 0 ? [] : references.slice();
      if (i > 0) refs.push(messageId);

      // Ensure most emails are within sync window, but allow some older if --include-older
      let daysAgo;
      if (INCLUDE_OLDER && SINCE_DAYS < 180 && Math.random() < 0.3) {
        // 30% older emails (between SINCE_DAYS and 180 days ago)
        daysAgo = SINCE_DAYS + randomInt(180 - SINCE_DAYS);
      } else {
        // 70% within sync window
        daysAgo = randomInt(SINCE_DAYS);
      }
      const date = new Date(Date.now() - daysAgo * 24 * 60 * 60 * 1000);
      const body = `Seed email ${created + 1} in thread ${t + 1}.\n\nThis is synthetic data for performance testing.`;
      const raw = buildRawEmail({
        from: `"Seed Sender ${t + 1}" <seed-${t}@local.test>`,
        to: SMTP_USER,
        subject: i === 0 ? subject : `Re: ${subject}`,
        date,
        messageId,
        inReplyTo,
        references: refs,
        body,
      });

      const mailbox = pickFolder(i, t);
      const flags = i % 7 === 0 ? [] : ['\\Seen'];
      await appendMessage(imap, mailbox, raw, flags);
      if (i > 0) references.push(messageId);
      previousMessageId = messageId;
      created += 1;

      if (created % 250 === 0) {
        console.log(`  seeded ${created}/${TOTAL}`);
      }
      if (created >= TOTAL) break;
    }
    if (created >= TOTAL) break;
  }

  await seedHtmlSamples(imap);
  await seedAttachmentSamples(imap);
  await seedProjectThreads(imap);
  console.log(`Done. Seeded ${created} messages.`);
  imap.end();
}

main().catch((err) => {
  console.error('[FATAL]', err);
  process.exit(1);
});
