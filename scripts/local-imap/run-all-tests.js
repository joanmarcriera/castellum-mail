#!/usr/bin/env node
/* SPDX-License-Identifier: Apache-2.0 */

/**
 * Greenmail test-bench runner
 *
 * Runs all local-imap integration tests in sequence.
 * Each test is a self-contained Node script that exits 0 on success.
 *
 * Prerequisites:
 *   docker compose -f scripts/local-imap/docker-compose.yml up -d
 *
 * Run:
 *   node scripts/local-imap/run-all-tests.js
 */

'use strict';

const { execSync } = require('child_process');
const path = require('path');

const TESTS = [
  { name: 'SMTP + IMAP smoke test',        script: 'test-smtp-greenmail.js' },
  { name: 'Attachment round-trip',          script: 'test-attachment-roundtrip.js' },
  { name: 'Inline image (cid:) resolution', script: 'test-inline-image-cid.js' },
];

const DIR = __dirname;
let passed = 0;
let failed = 0;
const failures = [];

console.log('╔══════════════════════════════════════════════════╗');
console.log('║   Castellum – Greenmail Integration Test Bench   ║');
console.log('╚══════════════════════════════════════════════════╝\n');

for (const test of TESTS) {
  process.stdout.write(`  ${test.name} … `);
  try {
    execSync(`node "${path.join(DIR, test.script)}"`, {
      stdio: ['pipe', 'pipe', 'pipe'],
      timeout: 15000,
    });
    console.log('✓');
    passed++;
  } catch (err) {
    console.log('✗');
    failures.push({ name: test.name, output: (err.stdout || err.stderr || err.message).toString().trim() });
    failed++;
  }
}

console.log(`\n─── Results: ${passed} passed, ${failed} failed ───`);

if (failures.length > 0) {
  console.log('\nFailure details:\n');
  for (const f of failures) {
    console.log(`  ● ${f.name}`);
    for (const line of f.output.split('\n').slice(-5)) {
      console.log(`      ${line}`);
    }
    console.log('');
  }
  process.exit(1);
}

console.log('\nAll tests passed.');
