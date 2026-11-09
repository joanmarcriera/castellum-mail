#!/usr/bin/env node
/* SPDX-License-Identifier: Apache-2.0 */

/**
 * Greenmail SMTP + IMAP smoke test
 *
 * Prerequisites:
 *   docker compose -f scripts/local-imap/docker-compose.yml up -d
 *
 * Run from repo root:
 *   node scripts/local-imap/test-smtp-greenmail.js
 *
 * Credentials (from docker-compose):
 *   user:     test@local.test
 *   password: secret
 *   SMTP:     localhost:3025 (no TLS)
 *   IMAP:     localhost:3143 (no TLS)
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

const SUBJECT = `[Castellum smoke] ${new Date().toISOString()}`;
const BODY = 'Hello from the Castellum SMTP smoke test.';

async function sendEmail() {
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
    text: BODY,
  });

  console.log('[SMTP] Message sent. Server response:', info.response);
  console.log('[SMTP] Message-ID:', info.messageId);
  transporter.close();
  return info.messageId;
}

function fetchEmails() {
  return new Promise((resolve, reject) => {
    const imap = new Imap({
      host: IMAP_HOST,
      port: IMAP_PORT,
      tls: false,
      user: USER,
      password: PASS,
    });

    imap.once('ready', () => {
      imap.openBox('INBOX', false, (err, box) => {
        if (err) return reject(err);

        // Search for our specific subject
        imap.search([['SUBJECT', '[Castellum smoke]']], (searchErr, uids) => {
          if (searchErr) return reject(searchErr);

          if (uids.length === 0) {
            imap.end();
            return resolve([]);
          }

          const found = [];
          const f = imap.fetch(uids, { bodies: '' });

          f.on('message', (msg) => {
            let buffer = '';
            msg.on('body', (stream) => {
              stream.on('data', (chunk) => {
                buffer += chunk.toString('utf8');
              });
              stream.once('end', () => {
                simpleParser(buffer, (parseErr, parsed) => {
                  if (!parseErr) {
                    found.push({
                      subject: parsed.subject,
                      from: parsed.from?.text,
                      text: parsed.text,
                    });
                  }
                });
              });
            });
          });

          f.once('error', reject);
          f.once('end', () => {
            imap.end();
            // Give simpleParser a tick to finish
            setTimeout(() => resolve(found), 100);
          });
        });
      });
    });

    imap.once('error', reject);
    imap.connect();
  });
}

async function main() {
  console.log('=== Castellum Greenmail SMTP + IMAP Smoke Test ===\n');

  // 1. Send
  console.log('[1/2] Sending email via SMTP...');
  try {
    await sendEmail();
  } catch (err) {
    console.error('[SMTP] FAILED:', err.message);
    process.exit(1);
  }

  // Small delay to let greenmail index the message
  await new Promise((r) => setTimeout(r, 500));

  // 2. Fetch back via IMAP
  console.log('[2/2] Fetching emails via IMAP...');
  let emails;
  try {
    emails = await fetchEmails();
  } catch (err) {
    console.error('[IMAP] FAILED:', err.message);
    process.exit(1);
  }

  // Validate
  const match = emails.find((e) => e.subject === SUBJECT);
  if (match) {
    console.log('\n[OK] Round-trip successful.');
    console.log('     Subject:', match.subject);
    console.log('     From:   ', match.from);
    console.log('     Body:   ', match.text?.trim());
  } else {
    console.log('\n[WARN] Email sent but not found in IMAP fetch.');
    console.log('       Found subjects:', emails.map((e) => e.subject));
    process.exit(1);
  }
}

main().catch((err) => {
  console.error('[FATAL]', err);
  process.exit(1);
});
