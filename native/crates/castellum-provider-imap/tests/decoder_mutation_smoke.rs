// Castellum - CI-gated "never panics" smoke for the IMAP response decoder (G6).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `native/fuzz` (cargo-fuzz, nightly, unbounded) is the real fuzzer and stays
// manual. This stable-toolchain test gives CI a bounded, deterministic slice
// of the same property on every push: imap-codec's ResponseCodec and
// GreetingCodec must return Ok or a typed Err for ANY bytes, never panic
// (CLAUDE.md gotcha 1 G6). It mutates real server responses
// (Gmail, Exchange-shaped, QRESYNC, literals) with a seeded xorshift PRNG:
// byte flips, truncations, splices, duplicated and inserted CRLFs, braces
// and digits that make bogus literal lengths. Same seed, same inputs, so a
// failure here reproduces exactly; feed the printed input to the fuzzer's
// corpus to minimise it.

use imap_codec::decode::Decoder;
use imap_codec::{GreetingCodec, ResponseCodec};

/// Representative server lines: greetings, untagged data, tagged status,
/// literals, and the Gmail shapes behind the 2026-07-11 crash.
const SEEDS: &[&[u8]] = &[
    b"* OK [CAPABILITY IMAP4rev1 IDLE NAMESPACE CONDSTORE QRESYNC] Gimap ready\r\n",
    b"* PREAUTH [CAPABILITY IMAP4rev1] logged in\r\n",
    b"* BYE server shutting down\r\n",
    b"* CAPABILITY IMAP4rev1 UNSELECT IDLE NAMESPACE QUOTA ID XLIST CHILDREN X-GM-EXT-1 UIDPLUS COMPRESS=DEFLATE ENABLE MOVE CONDSTORE ESEARCH UTF8=ACCEPT LIST-EXTENDED LIST-STATUS LITERAL- SPECIAL-USE APPENDLIMIT=35651584\r\n",
    b"* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n",
    b"* LIST (\\HasNoChildren \\Sent) \"/\" \"[Gmail]/Sent Mail\"\r\n",
    b"* STATUS INBOX (MESSAGES 231 UIDNEXT 44292 UIDVALIDITY 1 UNSEEN 3)\r\n",
    b"* 12 FETCH (UID 4827 FLAGS (\\Seen $label1) INTERNALDATE \"17-Jul-1996 02:44:25 -0700\" RFC822.SIZE 4286 MODSEQ (624140003))\r\n",
    b"* 3 FETCH (UID 9 BODY[HEADER.FIELDS (SUBJECT)] {21}\r\nSubject: Hello there\r\n)\r\n",
    b"* 1 FETCH (ENVELOPE (\"Mon, 7 Feb 1994 21:52:25 -0800\" \"=?utf-8?B?QsOgcmNlbG9uYQ==?=\" ((\"Fred\" NIL \"fred\" \"example.com\")) NIL NIL ((NIL NIL \"joe\" \"example.com\")) NIL NIL NIL \"<B27397-0100000@example.com>\"))\r\n",
    b"* VANISHED (EARLIER) 41,43:116,118,120:211,214:540\r\n",
    b"* OK [HIGHESTMODSEQ 715194045007] Highest\r\n",
    b"* SEARCH 2 84 882\r\n",
    b"* ESEARCH (TAG \"A282\") UID MIN 2 COUNT 3\r\n",
    b"+ idling\r\n",
    b"A142 OK [READ-WRITE] SELECT completed\r\n",
    b"A143 NO [TRYCREATE] Mailbox doesn't exist\r\n",
    b"A144 BAD Command unknown\r\n",
    b"* 5 EXPUNGE\r\n* 4 EXISTS\r\n",
];

/// xorshift64*: tiny, deterministic, no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Bytes that steer the decoder into its interesting branches.
const SPICE: &[u8] = b"{}()[]\"\\\r\n* 0123456789~+-:%";

fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut v = input.to_vec();
    for _ in 0..1 + rng.below(4) {
        let len = v.len();
        match rng.below(7) {
            0 if len > 0 => {
                let i = rng.below(len);
                v[i] ^= 1 << rng.below(8);
            }
            1 if len > 0 => v.truncate(rng.below(len)),
            2 => {
                let i = rng.below(len + 1);
                v.insert(i, SPICE[rng.below(SPICE.len())]);
            }
            3 if len > 1 => {
                let i = rng.below(len);
                v.remove(i);
            }
            4 => {
                // Splice another seed in: mixed and pipelined responses.
                let other = SEEDS[rng.below(SEEDS.len())];
                let i = rng.below(len + 1);
                let take = rng.below(other.len() + 1);
                v.splice(i..i, other[..take].iter().copied());
            }
            5 => {
                // A bogus literal length, the classic over-read bait.
                let i = rng.below(len + 1);
                let n = rng.next() % 100_000;
                v.splice(i..i, format!("{{{n}}}\r\n").into_bytes());
            }
            _ if len > 0 => {
                let i = rng.below(len);
                v[i] = (rng.next() & 0xFF) as u8;
            }
            _ => v.push(b'*'),
        }
    }
    v
}

#[test]
fn response_and_greeting_decoders_never_panic_on_mutated_server_bytes() {
    // Bounded so the suite stays fast: about 120k decodes, well under a second
    // in release and a few seconds in debug.
    const ROUNDS: usize = 3_000;
    let mut rng = Rng(0x0C45_7E11_0000_2026);
    for round in 0..ROUNDS {
        for seed in SEEDS {
            let input = mutate(&mut rng, seed);
            let outcome = std::panic::catch_unwind(|| {
                let _ = ResponseCodec::default().decode(&input);
                let _ = GreetingCodec::default().decode(&input);
            });
            assert!(
                outcome.is_ok(),
                "decoder panicked in round {round} on input {:?}",
                String::from_utf8_lossy(&input)
            );
        }
    }
}

#[test]
fn every_seed_decodes_without_panicking_as_is() {
    for seed in SEEDS {
        let _ = ResponseCodec::default().decode(seed);
        let _ = GreetingCodec::default().decode(seed);
    }
}
