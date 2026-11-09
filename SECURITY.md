<!-- SPDX-License-Identifier: Apache-2.0 -->

# Security policy

## Reporting a vulnerability

Please report security problems privately by email to **marc@riera.co.uk**.
Do not open a public issue or pull request for a vulnerability.

Include what you found, how to reproduce it, the affected version or commit,
and the impact as you understand it. Do not include real mail or credentials.

You can expect an acknowledgement within 5 working days. This is a one-person
project, so fix times vary with severity; I will keep you informed and credit you
in the fix notes unless you prefer otherwise.

## Supported versions

Only the latest commit on `main` and the latest tagged release receive fixes.

## Scope

In scope: the Rust core, the Swift app, credential and Keychain handling, mail
parsing and rendering (including remote-content and tracker handling), and the
local AI egress guard. Out of scope: vulnerabilities in third-party mail
providers or in a model server you run yourself.
