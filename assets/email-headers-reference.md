# Email Headers: Comprehensive Reference Guide

## Document Purpose

This document provides an extensive reference of email headers used in email transmission and processing. It is designed for LLM systems developing email client applications, with a focus on filtering, searching, and managing email messages according to RFC specifications and industry standards.

---

## Table of Contents

1. [Core RFC 5322 Headers](#core-rfc-5322-headers)
2. [Addressing Headers](#addressing-headers)
3. [Date and Time Headers](#date-and-time-headers)
4. [Message Identification Headers](#message-identification-headers)
5. [MIME and Content Headers](#mime-and-content-headers)
6. [Delivery and Routing Headers](#delivery-and-routing-headers)
7. [Authentication Headers](#authentication-headers)
8. [Spam and Security Headers](#spam-and-security-headers)
9. [Mailing List Headers](#mailing-list-headers)
10. [Tracking and Receipt Headers](#tracking-and-receipt-headers)
11. [Client-Specific Headers](#client-specific-headers)
12. [Priority and Importance Headers](#priority-and-importance-headers)
13. [Encryption and Security Headers](#encryption-and-security-headers)
14. [Proprietary and Extended Headers](#proprietary-and-extended-headers)
15. [Deprecated and Legacy Headers](#deprecated-and-legacy-headers)

---

## Core RFC 5322 Headers

These are the fundamental headers defined in RFC 5322 (Internet Message Format).

### From
- **Purpose**: Specifies the author(s) of the message
- **Format**: `From: "Display Name" <email@example.com>`
- **Required**: Yes
- **Multiple Values**: Yes (comma-separated)
- **Search/Filter Use**: Author identification, sender filtering, contact grouping
- **Example**: `From: John Smith <john.smith@company.com>`

### To
- **Purpose**: Primary recipients of the message
- **Format**: `To: "Display Name" <email@example.com>`
- **Required**: No (but typically present)
- **Multiple Values**: Yes (comma-separated)
- **Search/Filter Use**: Recipient filtering, direct message identification
- **Example**: `To: alice@example.com, bob@example.com`

### Cc (Carbon Copy)
- **Purpose**: Secondary recipients who receive a copy
- **Format**: `Cc: "Display Name" <email@example.com>`
- **Required**: No
- **Multiple Values**: Yes (comma-separated)
- **Search/Filter Use**: Secondary recipient filtering, transparency tracking
- **Example**: `Cc: manager@company.com`

### Bcc (Blind Carbon Copy)
- **Purpose**: Hidden recipients (not visible to other recipients)
- **Format**: `Bcc: "Display Name" <email@example.com>`
- **Required**: No
- **Multiple Values**: Yes (comma-separated)
- **Search/Filter Use**: Privacy-protected recipient filtering
- **Note**: Usually stripped by mail servers before delivery
- **Example**: `Bcc: secret@example.com`

### Subject
- **Purpose**: Brief summary of the message content
- **Format**: `Subject: Message topic`
- **Required**: No (but highly recommended)
- **Multiple Values**: No
- **Search/Filter Use**: Primary search field, topic categorisation, thread identification
- **Example**: `Subject: Q1 Financial Report - Action Required`

### Date
- **Purpose**: Date and time when message was composed
- **Format**: `Date: Day, DD Mon YYYY HH:MM:SS +ZZZZ`
- **Required**: Yes
- **Multiple Values**: No
- **Search/Filter Use**: Temporal filtering, chronological sorting, date-range searches
- **Example**: `Date: Thu, 05 Feb 2026 18:56:00 +0000`

### Reply-To
- **Purpose**: Address where replies should be sent (if different from From)
- **Format**: `Reply-To: email@example.com`
- **Required**: No
- **Multiple Values**: Yes (comma-separated)
- **Search/Filter Use**: Reply routing, mailing list filtering
- **Example**: `Reply-To: support@company.com`

### Sender
- **Purpose**: The agent responsible for sending (if different from author)
- **Format**: `Sender: email@example.com`
- **Required**: No (required if From has multiple addresses)
- **Multiple Values**: No
- **Search/Filter Use**: Authentication, actual sender identification
- **Example**: `Sender: secretary@company.com`

---

## Addressing Headers

### Resent-From
- **Purpose**: Original sender when message is resent
- **Format**: `Resent-From: email@example.com`
- **Search/Filter Use**: Forwarding chain tracking

### Resent-To
- **Purpose**: New recipients when message is resent
- **Format**: `Resent-To: email@example.com`
- **Search/Filter Use**: Resend tracking

### Resent-Cc
- **Purpose**: Copy recipients for resent message
- **Format**: `Resent-Cc: email@example.com`
- **Search/Filter Use**: Resend copy tracking

### Resent-Bcc
- **Purpose**: Blind copy recipients for resent message
- **Format**: `Resent-Bcc: email@example.com`
- **Search/Filter Use**: Hidden resend tracking

### Resent-Date
- **Purpose**: Date when message was resent
- **Format**: `Resent-Date: Day, DD Mon YYYY HH:MM:SS +ZZZZ`
- **Search/Filter Use**: Resend timestamp tracking

### Resent-Message-ID
- **Purpose**: New message ID for resent message
- **Format**: `Resent-Message-ID: <unique-id@domain.com>`
- **Search/Filter Use**: Resent message identification

### Resent-Sender
- **Purpose**: Agent who resent the message
- **Format**: `Resent-Sender: email@example.com`
- **Search/Filter Use**: Resend agent identification

### Return-Path
- **Purpose**: Email address for bounce messages
- **Format**: `Return-Path: <email@example.com>`
- **Search/Filter Use**: Bounce handling, delivery failure tracking
- **Example**: `Return-Path: <bounces@company.com>`

### Envelope-To
- **Purpose**: Original SMTP envelope recipient
- **Format**: `Envelope-To: email@example.com`
- **Search/Filter Use**: Actual delivery address identification

### Envelope-From
- **Purpose**: Original SMTP envelope sender
- **Format**: `Envelope-From: email@example.com`
- **Search/Filter Use**: Actual sender identification

---

## Date and Time Headers

### Date
- **Already covered in Core Headers**

### Expires
- **Purpose**: Date after which message is considered expired
- **Format**: `Expires: Day, DD Mon YYYY HH:MM:SS +ZZZZ`
- **Search/Filter Use**: Expiration filtering, automatic archival
- **Example**: `Expires: Fri, 06 Mar 2026 00:00:00 +0000`

### Expiry-Date
- **Purpose**: Alternative format for expiration
- **Format**: `Expiry-Date: Day, DD Mon YYYY HH:MM:SS +ZZZZ`
- **Search/Filter Use**: Expiration filtering

### Delivery-Date
- **Purpose**: Date when message was delivered to recipient's mailbox
- **Format**: `Delivery-Date: Day, DD Mon YYYY HH:MM:SS +ZZZZ`
- **Search/Filter Use**: Actual delivery time tracking

### Posted-Date
- **Purpose**: Date when message was posted (used in some systems)
- **Format**: `Posted-Date: Day, DD Mon YYYY HH:MM:SS +ZZZZ`
- **Search/Filter Use**: Posting time tracking

---

## Message Identification Headers

### Message-ID
- **Purpose**: Unique identifier for the message
- **Format**: `Message-ID: <unique-string@domain.com>`
- **Required**: Should be present
- **Multiple Values**: No
- **Search/Filter Use**: Unique message identification, deduplication, threading
- **Example**: `Message-ID: <20260205185600.12345@mail.example.com>`

### In-Reply-To
- **Purpose**: Message-ID of the message being replied to
- **Format**: `In-Reply-To: <message-id@domain.com>`
- **Search/Filter Use**: Thread reconstruction, conversation tracking
- **Example**: `In-Reply-To: <20260204120000.54321@mail.example.com>`

### References
- **Purpose**: Message-IDs of messages in the thread
- **Format**: `References: <id1@domain.com> <id2@domain.com>`
- **Multiple Values**: Yes (space-separated)
- **Search/Filter Use**: Complete thread tracking, conversation view
- **Example**: `References: <id1@example.com> <id2@example.com> <id3@example.com>`

### Original-Message-ID
- **Purpose**: Message-ID of original message when forwarding
- **Format**: `Original-Message-ID: <message-id@domain.com>`
- **Search/Filter Use**: Forward chain tracking

### Thread-Topic
- **Purpose**: Subject of the conversation thread
- **Format**: `Thread-Topic: Original subject`
- **Search/Filter Use**: Thread grouping, subject-based threading
- **Example**: `Thread-Topic: Budget Discussion`

### Thread-Index
- **Purpose**: Proprietary threading information (Microsoft)
- **Format**: `Thread-Index: Base64-encoded-value`
- **Search/Filter Use**: Microsoft Outlook threading

---

## MIME and Content Headers

### MIME-Version
- **Purpose**: MIME specification version
- **Format**: `MIME-Version: 1.0`
- **Required**: For MIME messages
- **Search/Filter Use**: MIME capability detection

### Content-Type
- **Purpose**: Media type of the message body
- **Format**: `Content-Type: type/subtype; parameters`
- **Common Values**: 
  - `text/plain; charset=utf-8`
  - `text/html; charset=utf-8`
  - `multipart/mixed`
  - `multipart/alternative`
  - `multipart/related`
- **Search/Filter Use**: Content format detection, HTML vs plain text filtering
- **Example**: `Content-Type: text/html; charset=UTF-8`

### Content-Transfer-Encoding
- **Purpose**: Encoding method for message body
- **Format**: `Content-Transfer-Encoding: encoding-type`
- **Common Values**: `7bit`, `8bit`, `binary`, `quoted-printable`, `base64`
- **Search/Filter Use**: Decoding information
- **Example**: `Content-Transfer-Encoding: base64`

### Content-Disposition
- **Purpose**: Presentation style (inline or attachment)
- **Format**: `Content-Disposition: disposition-type; parameters`
- **Common Values**: `inline`, `attachment; filename="file.pdf"`
- **Search/Filter Use**: Attachment detection, filename extraction
- **Example**: `Content-Disposition: attachment; filename="report.pdf"`

### Content-ID
- **Purpose**: Unique identifier for MIME part
- **Format**: `Content-ID: <unique-id@domain.com>`
- **Search/Filter Use**: Embedded content referencing

### Content-Description
- **Purpose**: Human-readable description of content
- **Format**: `Content-Description: Description text`
- **Search/Filter Use**: Attachment description

### Content-Location
- **Purpose**: URI for the content
- **Format**: `Content-Location: URI`
- **Search/Filter Use**: Content source tracking

### Content-Base
- **Purpose**: Base URI for resolving relative URIs
- **Format**: `Content-Base: URI`
- **Search/Filter Use**: URI resolution

### Content-Language
- **Purpose**: Language of the content
- **Format**: `Content-Language: en-GB`
- **Search/Filter Use**: Language-based filtering
- **Example**: `Content-Language: en-GB, ca-ES`

### Content-MD5
- **Purpose**: MD5 checksum of content
- **Format**: `Content-MD5: base64-encoded-hash`
- **Search/Filter Use**: Content integrity verification

---

## Delivery and Routing Headers

### Received
- **Purpose**: Trace of mail servers that handled the message
- **Format**: `Received: from server by server; date`
- **Multiple Values**: Yes (one per hop)
- **Search/Filter Use**: Route tracking, spam analysis, delivery verification
- **Example**: 
```
Received: from mail.sender.com (mail.sender.com [192.0.2.1])
    by mail.receiver.com (Postfix) with ESMTP id 12345
    for <recipient@receiver.com>; Thu, 05 Feb 2026 18:56:00 +0000
```

### Received-SPF
- **Purpose**: SPF authentication result
- **Format**: `Received-SPF: result`
- **Search/Filter Use**: SPF verification status
- **Example**: `Received-SPF: pass (sender IP authorized)`

### X-Originating-IP
- **Purpose**: IP address of the original sender
- **Format**: `X-Originating-IP: [IP address]`
- **Search/Filter Use**: Sender location, spam analysis
- **Example**: `X-Originating-IP: [192.0.2.1]`

### X-Mailer
- **Purpose**: Email client software used to send
- **Format**: `X-Mailer: Client Name Version`
- **Search/Filter Use**: Client identification, compatibility
- **Example**: `X-Mailer: Apple Mail (2.3654.60.0.1)`

### User-Agent
- **Purpose**: Email client identification (alternative to X-Mailer)
- **Format**: `User-Agent: Client Name/Version`
- **Search/Filter Use**: Client identification
- **Example**: `User-Agent: Mozilla/5.0 Thunderbird/115.0`

### X-Sender
- **Purpose**: Additional sender information
- **Format**: `X-Sender: email@example.com`
- **Search/Filter Use**: Sender verification

### X-Receiver
- **Purpose**: Additional receiver information
- **Format**: `X-Receiver: email@example.com`
- **Search/Filter Use**: Receiver tracking

### X-Priority
- **Purpose**: Message priority (0-5 scale, 1=highest)
- **Format**: `X-Priority: 1` (1=high, 3=normal, 5=low)
- **Search/Filter Use**: Priority-based filtering and sorting
- **Example**: `X-Priority: 1`

### Priority
- **Purpose**: Alternative priority header
- **Format**: `Priority: urgent`, `normal`, or `non-urgent`
- **Search/Filter Use**: Priority filtering

### Importance
- **Purpose**: Message importance indicator
- **Format**: `Importance: high`, `normal`, or `low`
- **Search/Filter Use**: Importance-based filtering
- **Example**: `Importance: high`

### X-MSMail-Priority
- **Purpose**: Microsoft priority indicator
- **Format**: `X-MSMail-Priority: High`, `Normal`, or `Low`
- **Search/Filter Use**: Microsoft client priority

### Auto-Submitted
- **Purpose**: Indicates auto-generated message
- **Format**: `Auto-Submitted: auto-generated`, `auto-replied`, or `no`
- **Search/Filter Use**: Automated message filtering
- **Example**: `Auto-Submitted: auto-replied`

### Precedence
- **Purpose**: Message precedence (bulk, list, junk, or first-class)
- **Format**: `Precedence: bulk`
- **Search/Filter Use**: Bulk mail filtering
- **Example**: `Precedence: bulk`

---

## Authentication Headers

### Authentication-Results
- **Purpose**: Consolidated authentication results
- **Format**: `Authentication-Results: server; methods and results`
- **Search/Filter Use**: Authentication status, security filtering
- **Example**: 
```
Authentication-Results: mx.google.com;
    dkim=pass header.i=@example.com;
    spf=pass smtp.mailfrom=example.com;
    dmarc=pass
```

### DKIM-Signature
- **Purpose**: DomainKeys Identified Mail signature
- **Format**: `DKIM-Signature: v=1; parameters; signature`
- **Search/Filter Use**: Email authentication, anti-spoofing
- **Example**: 
```
DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed;
    d=example.com; s=selector1; h=from:to:subject:date;
    bh=base64hash; b=base64signature
```

### DomainKey-Signature
- **Purpose**: Legacy DomainKeys signature (predecessor to DKIM)
- **Format**: `DomainKey-Signature: parameters; signature`
- **Search/Filter Use**: Legacy authentication

### ARC-Authentication-Results
- **Purpose**: Authenticated Received Chain authentication results
- **Format**: `ARC-Authentication-Results: i=1; server; results`
- **Search/Filter Use**: Forwarding chain authentication

### ARC-Message-Signature
- **Purpose**: ARC message signature
- **Format**: `ARC-Message-Signature: i=1; parameters; signature`
- **Search/Filter Use**: Message integrity in forwarding

### ARC-Seal
- **Purpose**: ARC seal for the chain
- **Format**: `ARC-Seal: i=1; parameters; signature`
- **Search/Filter Use**: Chain integrity verification

---

## Spam and Security Headers

### X-Spam-Status
- **Purpose**: Spam detection status
- **Format**: `X-Spam-Status: Yes/No, score=X`
- **Search/Filter Use**: Spam filtering, score-based filtering
- **Example**: `X-Spam-Status: No, score=-0.1 required=5.0`

### X-Spam-Score
- **Purpose**: Numerical spam score
- **Format**: `X-Spam-Score: number`
- **Search/Filter Use**: Spam probability filtering
- **Example**: `X-Spam-Score: 2.3`

### X-Spam-Level
- **Purpose**: Visual spam level indicator
- **Format**: `X-Spam-Level: ***` (asterisks = score)
- **Search/Filter Use**: Quick spam assessment

### X-Spam-Flag
- **Purpose**: Simple spam flag
- **Format**: `X-Spam-Flag: YES` or `NO`
- **Search/Filter Use**: Binary spam filtering
- **Example**: `X-Spam-Flag: NO`

### X-Spam-Report
- **Purpose**: Detailed spam analysis report
- **Format**: `X-Spam-Report: detailed rule matches`
- **Search/Filter Use**: Spam rule analysis

### X-Spam-Checker-Version
- **Purpose**: Spam checker software version
- **Format**: `X-Spam-Checker-Version: SpamAssassin 3.4.2`
- **Search/Filter Use**: Spam filter identification

### X-Virus-Scanned
- **Purpose**: Indicates virus scanning was performed
- **Format**: `X-Virus-Scanned: by antivirus software`
- **Search/Filter Use**: Security verification
- **Example**: `X-Virus-Scanned: ClamAV 0.103.2`

### X-Virus-Status
- **Purpose**: Virus scan result
- **Format**: `X-Virus-Status: Clean` or `Infected`
- **Search/Filter Use**: Virus filtering

### X-Forefront-Antispam-Report
- **Purpose**: Microsoft antispam analysis
- **Format**: `X-Forefront-Antispam-Report: detailed parameters`
- **Search/Filter Use**: Microsoft security filtering

### X-Microsoft-Antispam
- **Purpose**: Microsoft antispam metadata
- **Format**: `X-Microsoft-Antispam: BCL:0; PCL:0`
- **Search/Filter Use**: Microsoft spam filtering

### X-Proofpoint-Spam-Details
- **Purpose**: Proofpoint spam analysis
- **Format**: `X-Proofpoint-Spam-Details: rule=...; scores=...`
- **Search/Filter Use**: Proofpoint security filtering

### X-IronPort-Anti-Spam-Filtered
- **Purpose**: Cisco IronPort spam filtering
- **Format**: `X-IronPort-Anti-Spam-Filtered: true`
- **Search/Filter Use**: IronPort filtering status

### X-IronPort-Anti-Spam-Result
- **Purpose**: IronPort spam result
- **Format**: `X-IronPort-Anti-Spam-Result: detailed result`
- **Search/Filter Use**: IronPort spam analysis

---

## Mailing List Headers

### List-Id
- **Purpose**: Unique identifier for mailing list
- **Format**: `List-Id: Description <list-name.domain.com>`
- **Search/Filter Use**: Mailing list identification and filtering
- **Example**: `List-Id: Development Team <dev-team.company.com>`

### List-Help
- **Purpose**: URL or email for list help
- **Format**: `List-Help: <mailto:help@list.com> or <URL>`
- **Search/Filter Use**: List assistance access

### List-Unsubscribe
- **Purpose**: Unsubscribe method (URL or email)
- **Format**: `List-Unsubscribe: <mailto:unsub@list.com>, <URL>`
- **Search/Filter Use**: Unsubscribe functionality
- **Example**: `List-Unsubscribe: <https://list.com/unsub?id=123>`

### List-Unsubscribe-Post
- **Purpose**: One-click unsubscribe method
- **Format**: `List-Unsubscribe-Post: List-Unsubscribe=One-Click`
- **Search/Filter Use**: One-click unsubscribe detection

### List-Subscribe
- **Purpose**: Subscribe method
- **Format**: `List-Subscribe: <mailto:sub@list.com>, <URL>`
- **Search/Filter Use**: Subscription management

### List-Post
- **Purpose**: Address for posting to list
- **Format**: `List-Post: <mailto:list@domain.com>`
- **Search/Filter Use**: List posting address

### List-Owner
- **Purpose**: Contact information for list owner
- **Format**: `List-Owner: <mailto:owner@list.com>`
- **Search/Filter Use**: List administration contact

### List-Archive
- **Purpose**: URL for list archives
- **Format**: `List-Archive: <URL>`
- **Search/Filter Use**: Archive access

### Archived-At
- **Purpose**: Permalink to archived message
- **Format**: `Archived-At: <URL>`
- **Search/Filter Use**: Archive reference

### X-Mailing-List
- **Purpose**: Mailing list identifier (legacy)
- **Format**: `X-Mailing-List: list-name@domain.com`
- **Search/Filter Use**: List identification

### X-Loop
- **Purpose**: Loop detection for mailing lists
- **Format**: `X-Loop: list-name@domain.com`
- **Search/Filter Use**: Mail loop prevention

### X-BeenThere
- **Purpose**: Indicates message passed through list server
- **Format**: `X-BeenThere: list-name@domain.com`
- **Search/Filter Use**: List processing tracking

### X-Mailman-Version
- **Purpose**: Mailman software version
- **Format**: `X-Mailman-Version: 2.1.39`
- **Search/Filter Use**: List software identification

---

## Tracking and Receipt Headers

### Disposition-Notification-To
- **Purpose**: Request for read receipt
- **Format**: `Disposition-Notification-To: email@example.com`
- **Search/Filter Use**: Read receipt requests
- **Example**: `Disposition-Notification-To: sender@company.com`

### Return-Receipt-To
- **Purpose**: Alternative read receipt request
- **Format**: `Return-Receipt-To: email@example.com`
- **Search/Filter Use**: Receipt tracking

### X-Confirm-Reading-To
- **Purpose**: Request for reading confirmation
- **Format**: `X-Confirm-Reading-To: email@example.com`
- **Search/Filter Use**: Reading confirmation requests

### Disposition-Notification-Options
- **Purpose**: Options for MDN (Message Disposition Notification)
- **Format**: `Disposition-Notification-Options: parameters`
- **Search/Filter Use**: MDN configuration

### Original-Recipient
- **Purpose**: Original intended recipient (for DSN)
- **Format**: `Original-Recipient: rfc822;email@example.com`
- **Search/Filter Use**: Delivery status tracking

### Final-Recipient
- **Purpose**: Final recipient address (for DSN)
- **Format**: `Final-Recipient: rfc822;email@example.com`
- **Search/Filter Use**: Delivery status tracking

### Action
- **Purpose**: Action taken on message (for DSN)
- **Format**: `Action: failed`, `delayed`, `delivered`, `relayed`, `expanded`
- **Search/Filter Use**: Delivery action tracking

### Status
- **Purpose**: Delivery status code
- **Format**: `Status: code (2.x.x success, 4.x.x temp fail, 5.x.x perm fail)`
- **Search/Filter Use**: Delivery status analysis
- **Example**: `Status: 5.1.1 (Bad destination mailbox address)`

### Remote-MTA
- **Purpose**: Remote MTA involved in delivery
- **Format**: `Remote-MTA: dns; hostname`
- **Search/Filter Use**: Delivery chain tracking

### Diagnostic-Code
- **Purpose**: Diagnostic information for delivery failure
- **Format**: `Diagnostic-Code: smtp; error message`
- **Search/Filter Use**: Failure analysis

### Last-Attempt-Date
- **Purpose**: Last delivery attempt date
- **Format**: `Last-Attempt-Date: Day, DD Mon YYYY HH:MM:SS +ZZZZ`
- **Search/Filter Use**: Delivery attempt tracking

### Will-Retry-Until
- **Purpose**: Date until delivery retries will continue
- **Format**: `Will-Retry-Until: Day, DD Mon YYYY HH:MM:SS +ZZZZ`
- **Search/Filter Use**: Retry scheduling

---

## Client-Specific Headers

### X-Gmail-Labels
- **Purpose**: Gmail labels applied to message
- **Format**: `X-Gmail-Labels: Label1,Label2,Label3`
- **Search/Filter Use**: Gmail label filtering

### X-Gmail-Received
- **Purpose**: Gmail receipt timestamp
- **Format**: `X-Gmail-Received: unix-timestamp`
- **Search/Filter Use**: Gmail-specific timing

### X-Gm-Message-State
- **Purpose**: Gmail internal state
- **Format**: `X-Gm-Message-State: state-string`
- **Search/Filter Use**: Gmail synchronisation

### X-Google-Smtp-Source
- **Purpose**: Google SMTP source identifier
- **Format**: `X-Google-Smtp-Source: identifier`
- **Search/Filter Use**: Google routing information

### X-Microsoft-Exchange-Diagnostics
- **Purpose**: Exchange server diagnostics
- **Format**: `X-Microsoft-Exchange-Diagnostics: detailed data`
- **Search/Filter Use**: Exchange troubleshooting

### X-MS-Exchange-Organization-AuthAs
- **Purpose**: Exchange authentication method
- **Format**: `X-MS-Exchange-Organization-AuthAs: Internal/External/Anonymous`
- **Search/Filter Use**: Exchange auth tracking

### X-MS-Exchange-Organization-AuthSource
- **Purpose**: Exchange authentication source server
- **Format**: `X-MS-Exchange-Organization-AuthSource: server-name`
- **Search/Filter Use**: Exchange routing

### X-MS-Has-Attach
- **Purpose**: Indicates message has attachments (Microsoft)
- **Format**: `X-MS-Has-Attach: yes` or `no`
- **Search/Filter Use**: Attachment filtering

### X-MS-TNEF-Correlator
- **Purpose**: Microsoft TNEF correlation
- **Format**: `X-MS-TNEF-Correlator: correlation-id`
- **Search/Filter Use**: TNEF processing

### X-Yahoo-Newman-Id
- **Purpose**: Yahoo internal message ID
- **Format**: `X-Yahoo-Newman-Id: identifier`
- **Search/Filter Use**: Yahoo tracking

### X-Yahoo-Newman-Property
- **Purpose**: Yahoo message properties
- **Format**: `X-Yahoo-Newman-Property: property-data`
- **Search/Filter Use**: Yahoo classification

### X-AOL-IP
- **Purpose**: AOL originating IP
- **Format**: `X-AOL-IP: IP-address`
- **Search/Filter Use**: AOL sender tracking

### X-Apparently-To
- **Purpose**: Yahoo/AOL recipient address
- **Format**: `X-Apparently-To: email@example.com`
- **Search/Filter Use**: Recipient verification

### X-YMail-OSG
- **Purpose**: Yahoo Mail Outbound Security Gateway
- **Format**: `X-YMail-OSG: filter-data`
- **Search/Filter Use**: Yahoo security filtering

---

## Priority and Importance Headers

### X-Priority
- **Already covered in Delivery and Routing**

### Priority
- **Already covered in Delivery and Routing**

### Importance
- **Already covered in Delivery and Routing**

### X-MSMail-Priority
- **Already covered in Delivery and Routing**

### Urgency
- **Purpose**: Message urgency indicator
- **Format**: `Urgency: urgent`, `normal`, or `low`
- **Search/Filter Use**: Urgency-based filtering

### Sensitivity
- **Purpose**: Message sensitivity level
- **Format**: `Sensitivity: Personal`, `Private`, `Company-Confidential`
- **Search/Filter Use**: Confidentiality filtering
- **Example**: `Sensitivity: Private`

### X-Message-Flag
- **Purpose**: Message flag or importance (various formats)
- **Format**: `X-Message-Flag: flag-data`
- **Search/Filter Use**: Custom flagging systems

---

## Encryption and Security Headers

### Content-Transfer-Encoding
- **Already covered in MIME Headers**

### X-PGP-Key
- **Purpose**: PGP public key information
- **Format**: `X-PGP-Key: URL or fingerprint`
- **Search/Filter Use**: PGP key distribution

### X-PGP-Sig
- **Purpose**: PGP signature reference
- **Format**: `X-PGP-Sig: signature-data`
- **Search/Filter Use**: PGP verification

### X-GPG-Key
- **Purpose**: GPG public key information
- **Format**: `X-GPG-Key: URL or fingerprint`
- **Search/Filter Use**: GPG key distribution

### OpenPGP
- **Purpose**: OpenPGP key preferences
- **Format**: `OpenPGP: parameters`
- **Search/Filter Use**: OpenPGP configuration

### Autocrypt
- **Purpose**: Autocrypt encryption header
- **Format**: `Autocrypt: addr=email; keydata=base64`
- **Search/Filter Use**: Autocrypt key exchange
- **Example**: `Autocrypt: addr=user@example.com; keydata=mQENBF...`

### Protected-Headers
- **Purpose**: Indicates protected headers in encrypted message
- **Format**: `Protected-Headers: v=1`
- **Search/Filter Use**: Encryption metadata

### X-TLS-Required
- **Purpose**: Indicates TLS requirement
- **Format**: `X-TLS-Required: yes`
- **Search/Filter Use**: Transport security enforcement

---

## Proprietary and Extended Headers

### X-UIDL
- **Purpose**: Unique identifier for POP3
- **Format**: `X-UIDL: unique-id`
- **Search/Filter Use**: POP3 message tracking

### X-IMAP
- **Purpose**: IMAP-specific metadata
- **Format**: `X-IMAP: imap-data`
- **Search/Filter Use**: IMAP state tracking

### X-OfflineIMAP
- **Purpose**: OfflineIMAP synchronisation data
- **Format**: `X-OfflineIMAP: sync-data`
- **Search/Filter Use**: Offline sync tracking

### X-Evolution
- **Purpose**: Evolution mail client metadata
- **Format**: `X-Evolution: evolution-data`
- **Search/Filter Use**: Evolution client tracking

### X-Evolution-Source
- **Purpose**: Evolution source account
- **Format**: `X-Evolution-Source: account-id`
- **Search/Filter Use**: Evolution account management

### X-MimeOLE
- **Purpose**: Microsoft Outlook Express version
- **Format**: `X-MimeOLE: Produced By Microsoft MimeOLE V6.00.2900.2180`
- **Search/Filter Use**: Client identification

### X-Newsreader
- **Purpose**: Newsreader software identification
- **Format**: `X-Newsreader: Client Name Version`
- **Search/Filter Use**: Newsreader identification

### X-No-Archive
- **Purpose**: Request not to archive message
- **Format**: `X-No-Archive: yes`
- **Search/Filter Use**: Archive exclusion

### X-Notes-Item
- **Purpose**: Lotus Notes item data
- **Format**: `X-Notes-Item: item-data`
- **Search/Filter Use**: Lotus Notes tracking

### X-Forwarded-Message-Id
- **Purpose**: Message-ID of forwarded message
- **Format**: `X-Forwarded-Message-Id: <message-id@domain.com>`
- **Search/Filter Use**: Forward tracking

### X-Forwarded-For
- **Purpose**: Original sender in forwarding chain
- **Format**: `X-Forwarded-For: email@example.com`
- **Search/Filter Use**: Forward chain tracking

### X-Original-To
- **Purpose**: Original recipient before aliasing/forwarding
- **Format**: `X-Original-To: original@example.com`
- **Search/Filter Use**: Alias resolution

### X-Original-Sender
- **Purpose**: Original sender information
- **Format**: `X-Original-Sender: email@example.com`
- **Search/Filter Use**: Sender tracking through forwards

### X-Envelope-From
- **Purpose**: SMTP envelope sender
- **Format**: `X-Envelope-From: <email@example.com>`
- **Search/Filter Use**: Actual sender identification

### X-Envelope-To
- **Purpose**: SMTP envelope recipient
- **Format**: `X-Envelope-To: email@example.com`
- **Search/Filter Use**: Actual recipient identification

### X-Account-Key
- **Purpose**: Account identifier
- **Format**: `X-Account-Key: account-id`
- **Search/Filter Use**: Account-based filtering

### X-Uniform-Type-Identifier
- **Purpose**: Apple content type identifier
- **Format**: `X-Uniform-Type-Identifier: com.apple.mail-message`
- **Search/Filter Use**: Apple content classification

### X-Universally-Unique-Identifier
- **Purpose**: Apple unique message identifier
- **Format**: `X-Universally-Unique-Identifier: UUID`
- **Search/Filter Use**: Apple message identification

### X-Clacks-Overhead
- **Purpose**: GNU Terry Pratchett tribute header
- **Format**: `X-Clacks-Overhead: GNU Terry Pratchett`
- **Search/Filter Use**: Cultural/memorial header

### X-Face
- **Purpose**: Encoded black-and-white face image (48x48)
- **Format**: `X-Face: encoded-image-data`
- **Search/Filter Use**: Sender avatar display

### Face
- **Purpose**: Encoded colour face image (48x48 PNG)
- **Format**: `Face: base64-encoded-png`
- **Search/Filter Use**: Sender avatar display

### X-Image-URL
- **Purpose**: URL to sender's image
- **Format**: `X-Image-URL: https://example.com/avatar.jpg`
- **Search/Filter Use**: Avatar fetching

### X-Bugzilla-*
- **Purpose**: Bugzilla bug tracking headers (multiple variants)
- **Format**: Various
- **Search/Filter Use**: Bug tracking system integration

### X-RT-*
- **Purpose**: Request Tracker headers (multiple variants)
- **Format**: Various
- **Search/Filter Use**: Ticket system integration

### X-Ticket-*
- **Purpose**: Generic ticket system headers
- **Format**: Various
- **Search/Filter Use**: Support ticket integration

---

## Deprecated and Legacy Headers

### Errors-To
- **Purpose**: Error notification address (deprecated, use Return-Path)
- **Format**: `Errors-To: email@example.com`
- **Status**: Deprecated
- **Search/Filter Use**: Legacy bounce handling

### Apparently-To
- **Purpose**: Apparent recipient (non-standard, deprecated)
- **Format**: `Apparently-To: email@example.com`
- **Status**: Deprecated
- **Search/Filter Use**: Legacy recipient tracking

### X-UIDL
- **Purpose**: POP3 unique ID (legacy)
- **Format**: `X-UIDL: unique-id`
- **Status**: Legacy but still used

### Content-Class
- **Purpose**: Microsoft content classification
- **Format**: `Content-Class: urn:content-classes:message`
- **Status**: Legacy Microsoft header

### X-Mozilla-Status
- **Purpose**: Mozilla/Thunderbird status flags
- **Format**: `X-Mozilla-Status: hex-flags`
- **Search/Filter Use**: Thunderbird message state

### X-Mozilla-Status2
- **Purpose**: Extended Mozilla status flags
- **Format**: `X-Mozilla-Status2: hex-flags`
- **Search/Filter Use**: Thunderbird extended state

### X-Mozilla-Keys
- **Purpose**: Mozilla message tags/labels
- **Format**: `X-Mozilla-Keys: tag1 tag2`
- **Search/Filter Use**: Thunderbird tagging

### X-Status
- **Purpose**: Legacy status flags (read/replied)
- **Format**: `X-Status: R` (R=read, A=replied, F=flagged)
- **Search/Filter Use**: Legacy status tracking

### X-Keywords
- **Purpose**: Legacy keywords/tags
- **Format**: `X-Keywords: keyword1 keyword2`
- **Search/Filter Use**: Legacy tagging

### X-UID
- **Purpose**: Legacy unique identifier
- **Format**: `X-UID: number`
- **Search/Filter Use**: Legacy UID tracking

---

## Filtering and Search Implementation Guidelines

### High-Value Headers for Search

1. **Subject** - Full-text search, primary search target
2. **From** - Sender filtering, contact-based search
3. **To/Cc** - Recipient filtering
4. **Date** - Temporal filtering, date range searches
5. **Message-ID** - Unique identification, deduplication
6. **In-Reply-To/References** - Thread reconstruction
7. **Content-Type** - Content format filtering
8. **List-Id** - Mailing list filtering

### High-Value Headers for Filtering

1. **X-Spam-Status/Score** - Spam filtering
2. **Authentication-Results** - Security filtering
3. **X-Priority/Importance** - Priority filtering
4. **Content-Disposition** - Attachment filtering
5. **List-Unsubscribe** - Bulk mail identification
6. **Auto-Submitted** - Automated message filtering
7. **Received** - Route analysis, spam detection

### Metadata Extraction Priority

1. **Core addressing** (From, To, Cc, Bcc)
2. **Threading** (Message-ID, In-Reply-To, References)
3. **Timing** (Date, Received chain)
4. **Content** (Subject, Content-Type, attachments)
5. **Authentication** (DKIM, SPF, DMARC results)
6. **Classification** (spam scores, priority, list membership)

### Indexing Recommendations

- **Full-text index**: Subject, From (display name), To (display name)
- **Exact match index**: Message-ID, References, List-Id
- **Range index**: Date, X-Spam-Score
- **Flag index**: X-Spam-Flag, Importance, X-MS-Has-Attach
- **Tokenised index**: Received (domain extraction), Authentication-Results

---

## Header Parsing Considerations

### Case Sensitivity
- Header names are **case-insensitive** (Subject = SUBJECT = subject)
- Header values are typically case-sensitive (except for specific tokens)

### Whitespace Handling
- Leading/trailing whitespace should be trimmed
- Internal whitespace may be significant
- Header folding (multi-line headers) must be unfolded

### Encoding
- Headers may use RFC 2047 encoded-words: `=?charset?encoding?encoded-text?=`
- Example: `Subject: =?UTF-8?B?SGVsbG8gV29ybGQ=?=` (Base64)
- Example: `Subject: =?UTF-8?Q?Hello_World?=` (Quoted-Printable)

### Multiple Headers
- Some headers may appear multiple times (Received, Resent-*)
- Order matters for Received (topmost is most recent)
- Multiple values may use comma separation or multiple headers

### Header Continuation
- Long headers can be folded with CRLF followed by whitespace
- Must be unfolded for processing
- Example:
```
Subject: This is a very long subject line that
 continues on the next line
```

### Character Sets
- Modern emails should use UTF-8
- Legacy emails may use ISO-8859-1, Windows-1252, or other encodings
- Always check Content-Type charset parameter

---

## Security and Privacy Considerations

### Headers to Strip for Privacy
- **Bcc** - Always stripped by MTAs
- **X-Originating-IP** - May reveal sender location
- **Received** - May reveal internal network topology
- **User-Agent/X-Mailer** - May reveal client vulnerabilities

### Headers for Authentication Verification
- **DKIM-Signature** - Domain authentication
- **Authentication-Results** - Consolidated auth results
- **Received-SPF** - Sender authentication
- **ARC-*** - Forwarding authentication chain

### Headers for Spam Detection
- **Received** - Analyse routing, identify suspicious hops
- **X-Spam-*** - Explicit spam indicators
- **X-Originating-IP** - Compare against spam databases
- **Message-ID** - Check format validity
- **Date** - Detect backdating or future dating

---

## Edge Cases and Special Scenarios

### Forwarded Messages
- May contain **X-Forwarded-*** headers
- **Resent-*** headers for formal resending
- **Original-Message-ID** to track source

### Mailing Lists
- **List-*** headers for list management
- **Precedence: bulk** commonly used
- **X-BeenThere** to prevent loops

### Automated Messages
- **Auto-Submitted: auto-generated**
- **Precedence: bulk** or **list**
- May have **X-Auto-Response-Suppress** (Microsoft)

### Delivery Status Notifications (DSN)
- **Original-Recipient**
- **Final-Recipient**
- **Action** (failed, delayed, delivered, relayed, expanded)
- **Status** (SMTP status code)
- **Diagnostic-Code**

### Message Disposition Notifications (MDN)
- **Disposition-Notification-To** requests MDN
- **Disposition-Notification-Options** configures MDN
- MDN response contains **Original-Message-ID**, **Disposition**, **Final-Recipient**

---

## Conclusion

This comprehensive reference covers all major email headers used in modern email systems. When implementing email client functionality:

1. **Prioritise standard RFC headers** for maximum compatibility
2. **Support authentication headers** for security
3. **Implement spam filtering headers** for user protection
4. **Handle client-specific headers** for ecosystem compatibility
5. **Parse carefully** respecting encoding, folding, and character sets
6. **Index strategically** for efficient search and filtering
7. **Consider privacy** when displaying or storing header information

For LLM-based email client development, focus on extracting and indexing the high-value headers listed in the filtering guidelines section, whilst maintaining the ability to parse and display all headers for advanced users and debugging purposes.

---

## References

- RFC 5322 - Internet Message Format
- RFC 2045-2049 - MIME specifications
- RFC 6376 - DKIM Signatures
- RFC 7208 - SPF
- RFC 7489 - DMARC
- RFC 8617 - ARC
- RFC 2369 - Mailing List Headers
- RFC 3798 - Message Disposition Notifications
- RFC 3464 - Delivery Status Notifications

---

*Document Version: 1.0*
*Last Updated: 2026-02-05*
*Prepared for: LLM Email Client Development*