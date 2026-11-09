#!/usr/bin/env node
/* SPDX-License-Identifier: Apache-2.0 */

/**
 * GreenMail bulk seed script (Ollama-assisted)
 *
 * Usage:
 *   node scripts/local-imap/seed-greenmail-ollama.js --count 1000
 *   node scripts/local-imap/seed-greenmail-ollama.js --count 10000 --threads 500 --model llama3.2
 *
 * Prerequisites:
 *   docker compose -f scripts/local-imap/docker-compose.yml up -d
 *   Ollama running on localhost:11434
 */

'use strict';

const Imap = require('imap');
const http = require('http');
const fs = require('fs');
const path = require('path');

const SMTP_USER = 'test@local.test';
const SMTP_PASS = 'secret';
const IMAP_HOST = 'localhost';
const IMAP_PORT = 3143;

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
const MODEL = getArg('--model', 'llama3.2');
const BATCH = Math.max(5, parseInt(getArg('--batch', '25'), 10));
const SINCE_DAYS = Math.max(1, parseInt(getArg('--since-days', '7'), 10));
const INCLUDE_OLDER = args.includes('--include-older');

const HEADER_REF_PATH = path.resolve(__dirname, '../../assets/email-headers-reference.md');
const HEADER_EXAMPLES = (() => {
  try {
    const text = fs.readFileSync(HEADER_REF_PATH, 'utf8');
    const examples = [];
    const regex = /Example:\s*`([^`]+)`/g;
    let match;
    while ((match = regex.exec(text)) !== null) {
      if (match[1] && match[1].includes(':')) {
        examples.push(match[1].trim());
      }
    }
    const excluded = [
      'from:', 'to:', 'subject:', 'date:', 'message-id:', 'in-reply-to:',
      'references:', 'mime-version:', 'content-type:', 'content-transfer-encoding:'
    ];
    const filtered = examples.filter((line) => {
      const lower = line.toLowerCase();
      return !excluded.some((prefix) => lower.startsWith(prefix));
    });
    return Array.from(new Set(filtered));
  } catch (err) {
    console.warn('Failed to load email headers reference:', err.message || err);
    return [];
  }
})();

const FOLDERS = [
  'INBOX',
  'Sent',
  'Drafts',
  'Archive',
  'Archive_2025',
  'Personal',
  'Personal/2025',
  'Receipts',
  'Projects',
  'Newsletters',
];

// Structured scenarios for realistic workflow testing
const SCENARIOS = [
  {
    name: 'Meeting Notes - Action Items',
    threads: 2,
    messages: 4,
    type: 'meeting',
    subjects: ['Q1 Planning Meeting', 'Weekly Team Sync'],
    folders: ['INBOX', 'Projects'],
    tags: ['action-items', 'meeting-notes', 'deadline'],
  },
  {
    name: 'Invoice Dispute',
    threads: 1,
    messages: 5,
    type: 'invoice',
    subjects: ['Invoice #2025-0142 Discrepancy'],
    folders: ['INBOX', 'Receipts'],
    tags: ['finance', 'urgent', 'follow-up'],
  },
  {
    name: 'Travel Changes',
    threads: 1,
    messages: 3,
    type: 'travel',
    subjects: ['Flight Change Request - March Trip'],
    folders: ['INBOX', 'Personal'],
    tags: ['travel', 'booking', 'time-sensitive'],
  },
  {
    name: 'Contract Review',
    threads: 1,
    messages: 6,
    type: 'deadline',
    subjects: ['Vendor Agreement Review'],
    folders: ['INBOX', 'Archive'],
    tags: ['legal', 'deadline', 'review-required'],
  },
  {
    name: 'Hiring Pipeline',
    threads: 3,
    messages: 8,
    type: 'personal',
    subjects: ['Senior Engineer Candidate', 'Product Manager Interview', 'Designer Portfolio Review'],
    folders: ['INBOX', 'Sent', 'Archive'],
    tags: ['hiring', 'interview', 'candidates'],
  },
  {
    name: 'Incident Follow-up',
    threads: 1,
    messages: 7,
    type: 'action',
    subjects: ['[INC-2025-023] Database Performance Degradation'],
    folders: ['INBOX', 'Projects'],
    tags: ['incident', 'urgent', 'production'],
  },
  {
    name: 'Product Launch Coordination',
    threads: 2,
    messages: 10,
    type: 'followup',
    subjects: ['Mobile App v2.0 Launch', 'Marketing Materials Review'],
    folders: ['INBOX', 'Projects', 'Sent'],
    tags: ['launch', 'coordination', 'marketing'],
  },
  {
    name: 'Customer Escalation',
    threads: 1,
    messages: 5,
    type: 'action',
    subjects: ['[ESCALATED] Enterprise Client Data Export Issue'],
    folders: ['INBOX', 'Sent'],
    tags: ['customer', 'escalation', 'high-priority'],
  },
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
  subject,
  date,
  messageId,
  inReplyTo,
  references,
  extraHeaders,
  body,
}) {
  const headers = [];
  headers.push(`From: ${from}`);
  headers.push(`To: ${to}`);
  headers.push(`Subject: ${subject}`);
  headers.push(`Date: ${date.toUTCString()}`);
  headers.push(`Message-ID: ${messageId}`);
  if (inReplyTo) headers.push(`In-Reply-To: ${inReplyTo}`);
  if (references && references.length > 0) headers.push(`References: ${references.join(' ')}`);
  if (extraHeaders && extraHeaders.length > 0) {
    headers.push(...extraHeaders);
  }
  headers.push('MIME-Version: 1.0');
  headers.push('Content-Type: text/plain; charset=utf-8');
  headers.push('Content-Transfer-Encoding: 7bit');
  return `${headers.join('\r\n')}\r\n\r\n${body}\r\n`;
}

function randomInt(max) {
  return Math.floor(Math.random() * max);
}

function pickFolder(i, threadIndex) {
  const roll = (i + threadIndex) % 10;
  if (roll < 6) return 'INBOX';
  return FOLDERS[(i + threadIndex) % FOLDERS.length];
}

function maybe(prob) {
  return Math.random() < prob;
}

function pickOne(list) {
  return list[Math.floor(Math.random() * list.length)];
}

function pickMany(list, count) {
  const copy = list.slice();
  const picked = [];
  while (copy.length > 0 && picked.length < count) {
    const idx = Math.floor(Math.random() * copy.length);
    picked.push(copy.splice(idx, 1)[0]);
  }
  return picked;
}

function buildHeadersForItem(item, threadMeta, date, messageId, inReplyTo) {
  const headers = [];

  // Addressing / routing
  if (maybe(0.2)) headers.push(`Reply-To: ${item.reply_to || `reply+${threadMeta.threadId}@local.test`}`);
  if (maybe(0.1)) headers.push(`Sender: ${item.sender || `agent-${threadMeta.threadId}@local.test`}`);
  if (maybe(0.05)) headers.push(`Return-Path: <bounces-${threadMeta.threadId}@local.test>`);
  if (maybe(0.05)) headers.push(`Envelope-To: ${item.to_addr || SMTP_USER}`);

  // Mailing list headers
  if (item.type === 'mailing_list' || maybe(0.08)) {
    const listId = item.list_id || `list-${threadMeta.threadId}.local.test`;
    headers.push(`List-Id: <${listId}>`);
    headers.push(`List-Unsubscribe: <mailto:unsubscribe+${threadMeta.threadId}@local.test>`);
    headers.push(`List-Post: <mailto:post+${threadMeta.threadId}@local.test>`);
  }

  // Priority / importance
  if (item.type === 'deadline' || item.type === 'invoice' || maybe(0.1)) {
    headers.push(`Importance: High`);
    headers.push(`Priority: urgent`);
    headers.push(`X-Priority: 1`);
  } else if (maybe(0.1)) {
    headers.push(`Importance: Low`);
    headers.push(`Priority: non-urgent`);
    headers.push(`X-Priority: 5`);
  }

  // Client / misc
  if (maybe(0.4)) headers.push(`X-Mailer: Castellum Seed ${threadMeta.threadId}`);
  if (maybe(0.15)) headers.push(`X-Client-Tag: ${pickOne(['work', 'personal', 'urgent', 'followup', 'receipts'])}`);

  // Threading helpers
  if (inReplyTo) headers.push(`Thread-Topic: ${threadMeta.baseSubject}`);
  if (inReplyTo) headers.push(`Thread-Index: ${messageId.replace(/[<>]/g, '')}`);

  // Tracking / receipts
  if (maybe(0.05)) headers.push(`Disposition-Notification-To: ${item.from_addr || `seed-${threadMeta.threadId}@local.test`}`);
  if (maybe(0.05)) headers.push(`Return-Receipt-To: ${item.from_addr || `seed-${threadMeta.threadId}@local.test`}`);

  // Security / auth placeholders (synthetic)
  if (maybe(0.1)) headers.push(`Authentication-Results: local.test; dkim=pass header.d=local.test; spf=pass smtp.mailfrom=local.test`);
  if (maybe(0.1)) headers.push(`DKIM-Signature: v=1; a=rsa-sha256; d=local.test; s=default; c=relaxed/relaxed;`);

  // Resent headers (simulate forwarding)
  if (maybe(0.03)) {
    headers.push(`Resent-From: "Forwarder" <forwarder@local.test>`);
    headers.push(`Resent-To: ${item.to_addr || SMTP_USER}`);
    headers.push(`Resent-Date: ${date.toUTCString()}`);
    headers.push(`Resent-Message-ID: <resent-${threadMeta.threadId}-${Date.now()}@local.test>`);
  }

  // Add a few real-world headers from reference list
  if (HEADER_EXAMPLES.length > 0 && maybe(0.35)) {
    const extras = pickMany(HEADER_EXAMPLES, 1 + randomInt(3));
    headers.push(...extras);
  }

  return headers;
}

function postJson(path, payload) {
  const data = JSON.stringify(payload);
  const options = {
    hostname: 'localhost',
    port: 11434,
    path,
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'Content-Length': Buffer.byteLength(data),
    },
  };
  return new Promise((resolve, reject) => {
    const req = http.request(options, (res) => {
      let body = '';
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => resolve({ status: res.statusCode, body }));
    });
    req.on('error', reject);
    req.write(data);
    req.end();
  });
}

async function generateBatch(prompt) {
  const payload = {
    model: MODEL,
    prompt,
    stream: false,
  };
  const res = await postJson('/api/generate', payload);
  if (!res || res.status !== 200) {
    throw new Error(`Ollama generate failed: ${res?.status}`);
  }
  const parsed = JSON.parse(res.body);
  return parsed.response || '';
}

function buildPrompt(batchSize, threadIndexStart, scenario) {
  if (scenario) {
    return [
      'You are generating synthetic email snippets for test data.',
      'Return ONLY JSON array, no markdown fences.',
      `Create ${batchSize} items for scenario: ${scenario.name}.`,
      `Type: ${scenario.type}. Tags: ${scenario.tags.join(', ')}.`,
      `Base subject: ${scenario.subjects[0]}.`,
      'Each item has: subject (vary slightly from base), body (2-4 sentences, realistic and brief), from_name, from_addr (use @local.test or @example.com), to_addr, tags (array), type.',
      'Optional fields: reply_to, sender, list_id.',
      'Keep bodies concise and realistic.',
    ].join('\n');
  }
  return [
    'You are generating synthetic email snippets for test data.',
    'Return ONLY JSON array, no markdown.',
    `Create ${batchSize} items.`,
    'Each item has: subject, body, from_name, from_addr, to_addr, tags (array), type (one of: action, followup, info, deadline, travel, invoice, meeting, personal, mailing_list).',
    'Also include optional fields: reply_to, sender, list_id.',
    'Subjects should be short. Body should be 2-4 short sentences, realistic and brief.',
    'Make them varied and realistic.',
    `Thread index base: ${threadIndexStart}.`,
  ].join('\n');
}

function parseJsonSafe(text) {
  try {
    const start = text.indexOf('[');
    const end = text.lastIndexOf(']');
    if (start === -1 || end === -1) return [];
    const slice = text.slice(start, end + 1);
    return JSON.parse(slice);
  } catch {
    return [];
  }
}

async function main() {
  console.log('=== GreenMail Bulk Seed (Ollama) ===');
  console.log(`Target: ${TOTAL} emails in ${THREADS} threads`);
  console.log(`Model: ${MODEL} | Batch size: ${BATCH}`);
  console.log(`Date window: last ${SINCE_DAYS} days${INCLUDE_OLDER ? ' (+ 30% older)' : ''}`);

  const imap = await connectImap();
  for (const folder of FOLDERS) {
    await addBox(imap, folder);
  }

  let created = 0;

  // Seed structured scenarios first (if we have enough quota)
  const scenarioQuota = Math.min(TOTAL * 0.3, SCENARIOS.reduce((sum, s) => sum + s.threads * s.messages, 0));
  if (scenarioQuota > 0) {
    console.log(`Seeding ${SCENARIOS.length} structured scenarios...`);
    for (const scenario of SCENARIOS) {
      for (let st = 0; st < scenario.threads; st += 1) {
        const rootMessageId = `<scenario-${scenario.name.replace(/\s/g, '-')}-${st}@local.test>`;
        const references = [rootMessageId];
        const baseSubject = scenario.subjects[st % scenario.subjects.length];
        const targetFolder = scenario.folders[st % scenario.folders.length];
        let previousMessageId = rootMessageId;

        for (let si = 0; si < scenario.messages; si += 1) {
          const messageId = `<scenario-${scenario.name.replace(/\s/g, '-')}-${st}-${si}@local.test>`;
          const inReplyTo = si === 0 ? null : previousMessageId;
          const refs = si === 0 ? [] : references.slice();
          if (si > 0) refs.push(messageId);

          const prompt = buildPrompt(1, st, scenario);
          const response = await generateBatch(prompt);
          const items = parseJsonSafe(response);
          const item = items[0] || {
            subject: baseSubject,
            body: 'Scenario-based test email.',
            from_name: 'Scenario Sender',
            from_addr: 'scenario@local.test',
            to_addr: SMTP_USER,
            tags: scenario.tags,
            type: scenario.type,
          };

          let daysAgo;
          if (INCLUDE_OLDER && SINCE_DAYS < 180 && Math.random() < 0.3) {
            daysAgo = SINCE_DAYS + randomInt(180 - SINCE_DAYS);
          } else {
            daysAgo = randomInt(SINCE_DAYS);
          }
          const date = new Date(Date.now() - daysAgo * 24 * 60 * 60 * 1000);

          const threadMeta = { threadId: `${scenario.name}-${st}`, baseSubject };
          const extraHeaders = buildHeadersForItem(item, threadMeta, date, messageId, inReplyTo);
          const raw = buildRawEmail({
            from: `"${item.from_name}" <${item.from_addr}>`,
            to: item.to_addr || SMTP_USER,
            subject: si === 0 ? baseSubject : `Re: ${baseSubject}`,
            date,
            messageId,
            inReplyTo,
            references: refs,
            extraHeaders,
            body: `${item.body}\n\n[scenario:${scenario.name}][tags:${scenario.tags.join(',')}]`,
          });

          const flags = si % 5 === 0 ? [] : ['\\Seen'];
          await appendMessage(imap, targetFolder, raw, flags);
          if (si > 0) references.push(messageId);
          previousMessageId = messageId;
          created += 1;

          if (created >= TOTAL) break;
        }
        if (created >= TOTAL) break;
      }
      if (created >= TOTAL) break;
    }
    console.log(`  seeded ${created} scenario emails`);
  }

  // Fill remaining with general threads
  const remaining = TOTAL - created;
  const THREADS_REMAINING = Math.max(1, Math.floor(remaining / 5));
  const perThread = Math.floor(remaining / THREADS_REMAINING);
  const remainder = remaining % THREADS_REMAINING;

  for (let t = 0; t < THREADS_REMAINING; t += 1) {
    const threadCount = perThread + (t < remainder ? 1 : 0);
    const rootMessageId = `<seed-${t}-0@local.test>`;
    const references = [rootMessageId];
    let previousMessageId = rootMessageId;

    let batchCache = [];
    let batchIndex = 0;

    for (let i = 0; i < threadCount; i += 1) {
      if (batchCache.length === 0) {
        const prompt = buildPrompt(BATCH, t + batchIndex * BATCH, null);
        const response = await generateBatch(prompt);
        batchCache = parseJsonSafe(response);
        batchIndex += 1;
        if (!Array.isArray(batchCache) || batchCache.length === 0) {
          batchCache = [{
            subject: `Thread ${t + 1} Update`,
            body: 'Synthetic email content for load testing.',
            from_name: `Seed Sender ${t + 1}`,
            from_addr: `seed-${t}@local.test`,
            to_addr: SMTP_USER,
            tags: [],
            type: 'info',
          }];
        }
      }

      const item = batchCache.shift();
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
      const fromName = item.from_name || `Seed Sender ${t + 1}`;
      const fromAddr = item.from_addr || `seed-${t}@local.test`;
      const subject = i === 0 ? item.subject : `Re: ${item.subject}`;
      const body = `${item.body}\n\n[tags:${(item.tags || []).join(',')}]`;
      const threadMeta = { threadId: t + 1, baseSubject: item.subject || `Thread ${t + 1}` };
      const extraHeaders = buildHeadersForItem(item, threadMeta, date, messageId, inReplyTo);
      const raw = buildRawEmail({
        from: `"${fromName}" <${fromAddr}>`,
        to: item.to_addr || SMTP_USER,
        subject,
        date,
        messageId,
        inReplyTo,
        references: refs,
        extraHeaders,
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

  console.log(`Done. Seeded ${created} messages.`);
  imap.end();
}

main().catch((err) => {
  console.error('[FATAL]', err);
  process.exit(1);
});
