#!/usr/bin/env node
/* SPDX-License-Identifier: Apache-2.0 */

/**
 * Attachment round-trip test
 *
 * Sends an email with a file attachment via SMTP, fetches it back via IMAP,
 * and verifies that mailparser correctly extracts the attachment content.
 * This validates the core path used by AttachmentService.downloadAttachment().
 *
 * Prerequisites:
 *   docker compose -f scripts/local-imap/docker-compose.yml up -d
 *
 * Run:
 *   node scripts/local-imap/test-attachment-roundtrip.js
 */

'use strict';

const nodemailer = require('nodemailer');
const Imap = require('imap');
const { simpleParser } = require('mailparser');

const SMTP_HOST = 'localhost';
const SMTP_PORT = 3025;
const IMAP_HOST = 'localhost';
const IMAP_PORT = 3143;
const USER = 'test@local.test';
const PASS = 'secret';

const SUBJECT = `[AttachmentTest] ${new Date().toISOString()}`;
const ATTACHMENT_CONTENT = 'Hello from the attachment round-trip test!';
const ATTACHMENT_FILENAME = 'hello.txt';

async function sendWithAttachment() {
  const transporter = nodemailer.createTransport({
    host: SMTP_HOST,
    port: SMTP_PORT,
    secure: false,
    auth: { user: USER, pass: PASS },
  });

  const info = await transporter.sendMail({
    from: `"Castellum Test" <${USER}>`,
    to: USER,
    subject: SUBJECT,
    text: 'See attached file.',
    attachments: [
      {
        filename: ATTACHMENT_FILENAME,
        content: Buffer.from(ATTACHMENT_CONTENT),
        contentType: 'text/plain',
      },
    ],
  });

  console.log('[SMTP] Sent with attachment. Response:', info.response);
  transporter.close();
}

function fetchAndVerify() {
  return new Promise((resolve, reject) => {
    const imap = new Imap({
      host: IMAP_HOST,
      port: IMAP_PORT,
      tls: false,
      user: USER,
      password: PASS,
    });

    imap.once('ready', () => {
      imap.openBox('INBOX', true, (err) => {
        if (err) return reject(err);

        imap.search([['SUBJECT', SUBJECT]], (searchErr, uids) => {
          if (searchErr) return reject(searchErr);
          if (uids.length === 0) return reject(new Error('Message not found via IMAP search'));

          const fetch = imap.fetch([uids[uids.length - 1]], { bodies: '' });

          fetch.on('message', (msg) => {
            let buffer = '';
            msg.on('body', (stream) => {
              stream.on('data', (chunk) => { buffer += chunk.toString('utf8'); });
            });
            msg.once('end', async () => {
              try {
                const parsed = await simpleParser(buffer);
                const attachments = parsed.attachments || [];

                if (attachments.length === 0) {
                  return reject(new Error('No attachments found in parsed message'));
                }

                const att = attachments.find((a) => a.filename === ATTACHMENT_FILENAME);
                if (!att) {
                  return reject(new Error(`Attachment "${ATTACHMENT_FILENAME}" not found. Found: ${attachments.map(a => a.filename).join(', ')}`));
                }

                const received = att.content?.toString('utf8');
                if (received !== ATTACHMENT_CONTENT) {
                  return reject(new Error(`Content mismatch.\n  Expected: "${ATTACHMENT_CONTENT}"\n  Got:      "${received}"`));
                }

                console.log('[IMAP] Attachment filename:', att.filename);
                console.log('[IMAP] Attachment size:    ', att.size, 'bytes');
                console.log('[IMAP] Content verified:    ✓');
                imap.end();
                resolve();
              } catch (e) { reject(e); }
            });
          });

          fetch.once('error', reject);
        });
      });
    });

    imap.once('error', reject);
    imap.connect();
  });
}

async function main() {
  console.log('=== Castellum Attachment Round-Trip Test ===\n');

  console.log('[1/2] Sending email with attachment...');
  await sendWithAttachment();

  await new Promise((r) => setTimeout(r, 600));

  console.log('[2/2] Fetching and verifying attachment...');
  await fetchAndVerify();

  console.log('\n[OK] Attachment round-trip successful.');
}

main().catch((err) => {
  console.error('[FAIL]', err.message || err);
  process.exit(1);
});
