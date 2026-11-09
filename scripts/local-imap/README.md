<!-- SPDX-License-Identifier: Apache-2.0 -->

# Local IMAP Test Service

This is a tiny local IMAP/SMTP server for testing onboarding and sync.

## Start

```bash
docker compose -f scripts/local-imap/docker-compose.yml up -d
```

## Default credentials

- Email: `test@local.test`
- Password: `secret`
- IMAP (plain): `localhost:3143` **TLS OFF**
- IMAPS (TLS): `localhost:3993` **TLS ON**
- SMTP: `localhost:3025` (TLS OFF)

## Test accounts (personas)

All on `localhost:3143` (IMAP, TLS off) / `localhost:3025` (SMTP, TLS off).
Seed them with sample mail: `node scripts/local-imap/seed-personas.js`
(idempotent; re-run adds another batch). Passwords are dev-only fixtures.

| Email | Password | Role in the sample mail |
|---|---|---|
| `test@local.test` | `secret` | original smoke-test account (bulk seeder target) |
| `alice@local.test` | `alice-pass` | project lead: threads with bob/carol, invoices, a newsletter |
| `bob@local.test` | `bob-pass` | engineer: replies to alice, receipts, GitHub-style notifications |
| `carol@local.test` | `carol-pass` | freelancer: client mail, a Catalan/Spanish thread, calendar invites |
| `dave@local.test` | `dave-pass` | empty mailbox: exercises first-sync / no-mail states |

Add one in Castellum via Settings > Accounts > Add: IMAP host `127.0.0.1`,
port `3143`, TLS off; SMTP host `127.0.0.1`, port `3025`, TLS off.

## If auth fails

GreenMail only loads users at startup. After changing credentials, restart:

```bash
docker compose -f scripts/local-imap/docker-compose.yml down
docker compose -f scripts/local-imap/docker-compose.yml up -d
```

## Stop

```bash
docker compose -f scripts/local-imap/docker-compose.yml down
```

## Test bench

Run the full integration test suite against the local greenmail instance:

```bash
node scripts/local-imap/run-all-tests.js
```

## Bulk seed (performance testing)

Generate a large mailbox with threads for load testing. By default, emails are dated within the last 7 days to appear immediately in the app (which has a 7-day sync window by default).

### Basic seed (1000 emails)

```bash
node scripts/local-imap/seed-greenmail.js --count 1000
```

### Heavy load testing (10k emails)

```bash
node scripts/local-imap/seed-greenmail.js --count 10000 --threads 500
```

### Custom date window

Control how far back emails are dated:

```bash
# Last 30 days only
node scripts/local-imap/seed-greenmail.js --count 1000 --since-days 30

# Last 7 days + 30% older emails (scattered across 180 days)
node scripts/local-imap/seed-greenmail.js --count 1000 --include-older
```

**Recommended for first-time testing**: Use default flags so emails appear immediately without needing "Sync all once".

## Bulk seed with Ollama (richer content)

If you have Ollama running locally, you can generate more realistic email content with structured scenarios.

### Quick start (1000 emails with scenarios)

```bash
node scripts/local-imap/seed-greenmail-ollama.js --count 1000
```

### With specific model

```bash
node scripts/local-imap/seed-greenmail-ollama.js --count 1000 --model llama3.2
```

### Heavy load with older emails

```bash
node scripts/local-imap/seed-greenmail-ollama.js --count 10000 --threads 500 --include-older
```

### Structured scenarios

The Ollama seeder includes 8 structured scenarios for realistic workflow testing:
- Meeting Notes - Action Items
- Invoice Dispute
- Travel Changes
- Contract Review
- Hiring Pipeline
- Incident Follow-up
- Product Launch Coordination
- Customer Escalation

Each scenario creates properly-threaded emails with realistic headers, spread across multiple folders (INBOX, Sent, Drafts, Archive, Personal, etc.).

The seeder synthesizes a wide variety of headers (Reply-To, Sender, List-Id, List-Unsubscribe, Importance, X-Priority, Resent-* and more) based on `assets/email-headers-reference.md` to exercise filtering and threading.

### Flags reference

| Flag | Default | Description |
|------|---------|-------------|
| `--count` | 1000 | Total number of emails to generate |
| `--threads` | auto | Number of conversation threads |
| `--model` | llama3.2 | Ollama model to use |
| `--batch` | 25 | Batch size for Ollama generation |
| `--since-days` | 7 | Date window (days back from now) |
| `--include-older` | off | Add 30% older emails beyond window |

Note: The seeding scripts do not pass IMAP INTERNALDATE due to a node-imap compatibility issue on newer Node versions. The Date header inside each message still reflects the intended timestamp.

Individual tests can also be run standalone:

| Script | What it checks |
| --- | --- |
| `test-smtp-greenmail.js` | Basic SMTP → IMAP round-trip (plain text) |
| `test-attachment-roundtrip.js` | File attachment survives SMTP → IMAP → mailparser extraction |
| `test-inline-image-cid.js` | Inline `cid:` image references are resolved to `data:` URIs |

## Notes
- This uses GreenMail for a self‑contained IMAP + SMTP service.
- If ports are in use, change them in the compose file and in onboarding.
