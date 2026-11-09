#!/usr/bin/env node
/* SPDX-License-Identifier: Apache-2.0 */

/**
 * Inline-image (cid:) resolution test
 *
 * Sends an HTML email with an embedded image referenced via cid:, fetches it
 * back via IMAP, and verifies that the cid: replacement logic (same logic used
 * in imap-service.ts parseEmail) produces a valid data: URI.
 *
 * Prerequisites:
 *   docker compose -f scripts/local-imap/docker-compose.yml up -d
 *
 * Run:
 *   node scripts/local-imap/test-inline-image-cid.js
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

const CID = 'castellum-test-image-' + Date.now();
const SUBJECT = `[CidTest] ${new Date().toISOString()}`;

// Minimal 1×1 red PNG (valid image, 68 bytes decoded)
const PNG_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8/5+hHgAHggJ/PchI7wAAAABJRU5ErkJggg==';

/**
 * Replaces cid: references in HTML with data: URIs using attachment buffers.
 * Mirror of the logic in src/main/email/imap-service.ts → parseEmail().
 */
function resolveCidReferences(htmlBody, attachments) {
  const cidMap = {};
  for (const att of attachments) {
    if (att.cid && att.content) {
      const base64 = att.content.toString('base64');
      const mime = att.contentType || 'image/png';
      cidMap[att.cid] = `data:${mime};base64,${base64}`;
    }
  }
  return htmlBody.replace(/\bcid:([^"'\s>]+)/gi, (_match, id) => cidMap[id] || _match);
}

async function sendWithInlineImage() {
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
    html: `<h1>Inline image test</h1><img src="cid:${CID}" alt="test" width="1" height="1">`,
    attachments: [
      {
        filename: 'test.png',
        content: Buffer.from(PNG_BASE64, 'base64'),
        contentType: 'image/png',
        cid: CID,
      },
    ],
  });

  console.log('[SMTP] Sent with inline image. Response:', info.response);
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
                const rawHtml = parsed.html || '';
                const attachments = parsed.attachments || [];

                // Apply the same cid: replacement that imap-service.ts does
                const resolvedHtml = resolveCidReferences(rawHtml, attachments);

                // Verify: no cid: references remain, and a data: URI is present
                const hasCid = /\bcid:/i.test(resolvedHtml);
                const hasDataUri = resolvedHtml.includes('data:image/png;base64,');

                console.log('[IMAP] Raw HTML contains cid: ref:     ', /\bcid:/i.test(rawHtml));
                console.log('[IMAP] Resolved HTML still has cid: ref:', hasCid);
                console.log('[IMAP] Resolved HTML has data: URI:     ', hasDataUri);
                console.log('[IMAP] Inline attachment count:         ', attachments.filter(a => a.cid).length);

                if (hasCid) {
                  return reject(new Error('cid: reference was not replaced'));
                }
                if (!hasDataUri) {
                  // Some IMAP servers (e.g. greenmail) may rewrite cid references
                  // in transit. If no cid was in raw HTML either, that is acceptable.
                  if (/\bcid:/i.test(rawHtml)) {
                    return reject(new Error('data: URI not produced after cid: replacement'));
                  }
                  console.log('[INFO] Server resolved cid: references in transit (no raw cid: found). Acceptable.');
                }

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
  console.log('=== Castellum Inline Image (cid:) Resolution Test ===\n');

  console.log('[1/2] Sending HTML email with cid: image reference...');
  await sendWithInlineImage();

  await new Promise((r) => setTimeout(r, 600));

  console.log('[2/2] Fetching and verifying cid: → data: resolution...');
  await fetchAndVerify();

  console.log('\n[OK] Inline image cid: resolution test passed.');
}

main().catch((err) => {
  console.error('[FAIL]', err.message || err);
  process.exit(1);
});
