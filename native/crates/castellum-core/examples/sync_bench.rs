// Castellum - first-sync performance baseline harness.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Runs the REAL sync entry point (`Profile::sync_imap_json`, exactly what the
// FFI's `castellum_sync_account` calls) against an IMAP server into a fresh
// temporary profile, with `castellum_core::sync_timing` enabled, and prints
// per-phase KPIs. Results feed project-docs/perf/2026-09-sync-baseline.md.
//
// Usage (credentials come from the environment or a dotenv file, never argv):
//
//   # synthetic corpus on a dedicated GreenMail (see the baseline doc)
//   BENCH_HOST=127.0.0.1 BENCH_PORT=4143 BENCH_TLS=0 \
//   BENCH_USER=perf@local.test BENCH_PASS=secret \
//     cargo run --release -p castellum-core --example sync_bench
//
//   # real account, credentials read from a dotenv file by key name
//   BENCH_HOST=imap.gmail.com BENCH_PORT=993 BENCH_DOTENV=../.env \
//   BENCH_USER_KEY=GOOGLE_GMAIL_ACCOUNT BENCH_PASS_KEY=GOOGLE_GMAIL_APP_PASSWORD \
//     cargo run --release -p castellum-core --example sync_bench
//
// Optional knobs:
//   BENCH_MAILBOXES=INBOX,Archive  restrict the sync (default: default sync set)
//   BENCH_PASSES=2                 1 = first sync only; 2 adds a no-op resync
//   BENCH_READER=1                 run a concurrent "UI read" thread (500-row
//                                  message list + mailbox counts every 50 ms)
//                                  and report its latency while sync runs
//   BENCH_WIRE_PROBE=INBOX         also run a raw-socket probe of that mailbox:
//                                  wire time + bytes for the descriptor FETCH,
//                                  and imap-codec parse time over the bytes
//
// READ-ONLY by construction: the sync path issues LIST/LSUB/SELECT/UID FETCH
// only (BODY.PEEK, never STORE/MOVE/EXPUNGE/APPEND), and the wire probe uses
// EXAMINE (read-only select) + UID FETCH. Output never contains message
// content — only mailbox names, counts, byte totals and timings.

use castellum_core::{open_profile, sync_timing, Profile};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Minimal KEY=VALUE dotenv reader (optional `export `, optional quotes).
fn dotenv(path: &str) -> HashMap<String, String> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    text.lines()
        .filter_map(|l| {
            let l = l.trim().trim_start_matches("export ");
            let (k, v) = l.split_once('=')?;
            if k.starts_with('#') {
                return None;
            }
            let v = v.trim().trim_matches('"').trim_matches('\'');
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

struct Target {
    host: String,
    port: u16,
    tls: bool,
    user: String,
    pass: String,
}

fn target() -> Target {
    let host = env("BENCH_HOST").unwrap_or_else(|| "127.0.0.1".into());
    let port = env("BENCH_PORT").map_or(4143, |p| p.parse().expect("BENCH_PORT"));
    let tls = env("BENCH_TLS").map_or(port == 993, |v| v == "1");
    let (user, pass) = match env("BENCH_DOTENV") {
        Some(path) => {
            let vars = dotenv(&path);
            let uk = env("BENCH_USER_KEY").expect("BENCH_USER_KEY");
            let pk = env("BENCH_PASS_KEY").expect("BENCH_PASS_KEY");
            (
                vars.get(&uk).cloned().expect("user key missing"),
                vars.get(&pk).cloned().expect("pass key missing"),
            )
        }
        None => (
            env("BENCH_USER").expect("BENCH_USER"),
            env("BENCH_PASS").expect("BENCH_PASS"),
        ),
    };
    Target {
        host,
        port,
        tls,
        user,
        pass,
    }
}

// --- stats ------------------------------------------------------------------

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn ms(us: u64) -> String {
    format!("{:.2}", us as f64 / 1000.0)
}

fn print_phase_table(t: &sync_timing::SyncTimings) {
    println!(
        "  {:<24} {:>7} {:>11} {:>9} {:>9} {:>9}",
        "phase", "n", "total_ms", "p50_ms", "p95_ms", "max_ms"
    );
    for (phase, samples) in &t.samples {
        let mut s = samples.clone();
        s.sort_unstable();
        println!(
            "  {:<24} {:>7} {:>11} {:>9} {:>9} {:>9}",
            phase,
            s.len(),
            ms(s.iter().sum()),
            ms(pct(&s, 0.5)),
            ms(pct(&s, 0.95)),
            ms(*s.last().unwrap_or(&0))
        );
    }
    // Per-100-message cost of descriptor persistence. Since,
    // `db.upsert_descriptor` records ONE sample per
    // `castellum_core::sync::DESCRIPTOR_UPSERT_BATCH_SIZE`-message
    // transaction, not one per message -- scale each batch's total linearly
    // (assumes uniform per-message cost within a batch) instead of
    // re-chunking already-batched samples into groups of 100, which would
    // silently aggregate whole multiples of the real batch size.
    if let Some(ups) = t.samples.get("db.upsert_descriptor") {
        let mut per100: Vec<u64> = ups
            .iter()
            .map(|&batch_total| {
                batch_total * 100 / castellum_core::sync::DESCRIPTOR_UPSERT_BATCH_SIZE as u64
            })
            .collect();
        per100.sort_unstable();
        println!(
            "  db.upsert_descriptor per 100 msgs: p50 {} ms, p95 {} ms ({} tx of {} msgs each)",
            ms(pct(&per100, 0.5)),
            ms(pct(&per100, 0.95)),
            per100.len(),
            castellum_core::sync::DESCRIPTOR_UPSERT_BATCH_SIZE
        );
    }
}

// --- concurrent UI-read probe -------------------------------------------------

fn spawn_reader(
    profile: Arc<Profile>,
    account_id: String,
    mailbox_id: String,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Vec<u64>> {
    std::thread::spawn(move || {
        let mut lat = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let t = Instant::now();
            let _ = profile.list_messages_json(&account_id, &mailbox_id, 500, 0);
            let _ = profile.mailbox_counts_json(&account_id);
            lat.push(t.elapsed().as_micros() as u64);
            std::thread::sleep(Duration::from_millis(50));
        }
        lat
    })
}

// --- raw wire probe -----------------------------------------------------------

trait Rw: Read + Write {}
impl<T: Read + Write> Rw for T {}

/// Read one logical IMAP response (a line plus any `{N}` literals it
/// announces) onto `buf`. Returns the byte offset where it started.
fn read_logical(r: &mut BufReader<Box<dyn Rw>>, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let start = buf.len();
    loop {
        let line_start = buf.len();
        let n = r.read_until(b'\n', buf)?;
        if n == 0 {
            return Err(std::io::Error::other("eof"));
        }
        let line = &buf[line_start..];
        // Literal announcement: "...{123}\r\n" -> read exactly 123 bytes.
        let lit = line
            .strip_suffix(b"\r\n")
            .and_then(|l| l.strip_suffix(b"}"))
            .and_then(|l| {
                let open = l.iter().rposition(|&b| b == b'{')?;
                std::str::from_utf8(&l[open + 1..])
                    .ok()?
                    .parse::<usize>()
                    .ok()
            });
        match lit {
            Some(len) => {
                let at = buf.len();
                buf.resize(at + len, 0);
                r.read_exact(&mut buf[at..])?;
            }
            None => return Ok(start),
        }
    }
}

/// Send `tag cmd`, read until the tagged completion. Returns (response bytes,
/// time to first byte, total time).
fn command(
    r: &mut BufReader<Box<dyn Rw>>,
    tag: &str,
    cmd: &str,
) -> std::io::Result<(Vec<u8>, Duration, Duration)> {
    let t0 = Instant::now();
    r.get_mut()
        .write_all(format!("{tag} {cmd}\r\n").as_bytes())?;
    r.get_mut().flush()?;
    let mut buf = Vec::new();
    let mut ttfb = None;
    loop {
        let start = read_logical(r, &mut buf)?;
        ttfb.get_or_insert_with(|| t0.elapsed());
        if buf[start..].starts_with(format!("{tag} ").as_bytes()) {
            return Ok((buf, ttfb.unwrap_or_default(), t0.elapsed()));
        }
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Parse `buf` with imap-codec (the decoder `Session` uses since S29), response
/// by response. Returns (responses parsed, parse errors, parse time).
fn parse_all(buf: &[u8]) -> (usize, usize, Duration) {
    use imap_codec::decode::Decoder;
    let codec = imap_codec::ResponseCodec::default();
    let t = Instant::now();
    let (mut pos, mut ok, mut errs) = (0usize, 0usize, 0usize);
    while pos < buf.len() {
        match codec.decode(&buf[pos..]) {
            Ok((rest, _)) => {
                ok += 1;
                pos = buf.len() - rest.len();
            }
            Err(_) => {
                errs += 1;
                // Resync at the next untagged response.
                match buf[pos + 1..].windows(3).position(|w| w == b"\n* ") {
                    Some(i) => pos = pos + 1 + i + 1,
                    None => break,
                }
            }
        }
    }
    (ok, errs, t.elapsed())
}

fn wire_probe(tg: &Target, mailbox: &str) -> std::io::Result<()> {
    let tcp = TcpStream::connect((tg.host.as_str(), tg.port))?;
    tcp.set_nodelay(true)?;
    let stream: Box<dyn Rw> = if tg.tls {
        let c = native_tls::TlsConnector::new().map_err(std::io::Error::other)?;
        Box::new(c.connect(&tg.host, tcp).map_err(std::io::Error::other)?)
    } else {
        Box::new(tcp)
    };
    let mut r = BufReader::with_capacity(1 << 20, stream);
    let mut greet = Vec::new();
    read_logical(&mut r, &mut greet)?;
    let (_, _, login) = command(
        &mut r,
        "p1",
        &format!("LOGIN {} {}", quote(&tg.user), quote(&tg.pass)),
    )?;
    let (_, _, examine) = command(&mut r, "p2", &format!("EXAMINE {}", quote(mailbox)))?;
    // NOOP round trip = pure network + server latency floor.
    let mut noop = Vec::new();
    for i in 0..10 {
        let (_, _, d) = command(&mut r, &format!("n{i}"), "NOOP")?;
        noop.push(d.as_micros() as u64);
    }
    noop.sort_unstable();
    println!("wire probe [{mailbox}]");
    println!(
        "  LOGIN {} ms, EXAMINE {} ms, NOOP RTT p50 {} ms / max {} ms",
        ms(login.as_micros() as u64),
        ms(examine.as_micros() as u64),
        ms(pct(&noop, 0.5)),
        ms(*noop.last().unwrap())
    );
    for (tag, items) in [
        (
            "p3",
            "(UID FLAGS RFC822.SIZE ENVELOPE INTERNALDATE BODYSTRUCTURE)",
        ),
        ("p4", "(UID FLAGS)"),
    ] {
        let (buf, ttfb, total) = command(&mut r, tag, &format!("UID FETCH 1:* {items}"))?;
        let (ok, errs, parse) = parse_all(&buf);
        let mb = buf.len() as f64 / 1_048_576.0;
        println!(
            "  UID FETCH 1:* {items}\n    {:.1} MiB, {} responses ({} unparseable), ttfb {} ms, wire total {} ms ({:.1} MiB/s), imap-proto parse {} ms (x1; the imap crate parses twice)",
            mb,
            ok,
            errs,
            ms(ttfb.as_micros() as u64),
            ms(total.as_micros() as u64),
            mb / total.as_secs_f64(),
            ms(parse.as_micros() as u64),
        );
    }
    let _ = command(&mut r, "p9", "LOGOUT");
    Ok(())
}

// --- main ---------------------------------------------------------------------

fn dir_size(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.metadata().map_or(0, |m| m.len()))
                .sum()
        })
        .unwrap_or(0)
}

fn main() {
    let tg = target();
    let account_id = "bench-acc".to_string();
    let mailboxes: Option<Vec<String>> =
        env("BENCH_MAILBOXES").map(|m| m.split(',').map(|s| s.trim().to_string()).collect());
    let passes: usize = env("BENCH_PASSES").map_or(2, |p| p.parse().expect("BENCH_PASSES"));
    let reader = env("BENCH_READER").is_some_and(|v| v == "1");

    let cfg = serde_json::json!({
        "account_id": account_id,
        "email": tg.user,
        "host": tg.host,
        "port": tg.port,
        "username": tg.user,
        "password": tg.pass,
        "use_tls": tg.tls,
        "allow_invalid_certs": false,
        "mailboxes": mailboxes,
    })
    .to_string();

    let dir = tempfile::tempdir().expect("tempdir");
    let profile = Arc::new(open_profile(dir.path()).expect("open profile"));
    println!(
        "target {}:{} tls={} mailboxes={:?}",
        tg.host, tg.port, tg.tls, mailboxes
    );

    for pass in 1..=passes {
        let stop = Arc::new(AtomicBool::new(false));
        let reader_handle = reader.then(|| {
            spawn_reader(
                profile.clone(),
                account_id.clone(),
                format!("{account_id}:INBOX"),
                stop.clone(),
            )
        });
        sync_timing::enable();
        let t = Instant::now();
        let result = profile.sync_imap_json(&cfg);
        let wall = t.elapsed();
        let timings = sync_timing::take().unwrap_or_default();
        stop.store(true, Ordering::Relaxed);
        let reader_lat = reader_handle.map(|h| h.join().unwrap());

        let label = if pass == 1 {
            "first sync"
        } else {
            "no-op resync"
        };
        println!(
            "\n== pass {pass} ({label}): wall {:.2} s",
            wall.as_secs_f64()
        );
        match &result {
            Ok(json) => {
                let v: serde_json::Value = serde_json::from_str(json).unwrap();
                let mut fetched = 0i64;
                for mb in v["mailboxes"].as_array().unwrap() {
                    let f = mb["fetched"].as_i64().unwrap_or(0);
                    fetched += f;
                    let err = mb["error"].as_str().map(|e| {
                        // Error class only; never echo server text verbatim.
                        e.split(':')
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(40)
                            .collect::<String>()
                    });
                    println!(
                        "  mailbox {:<28} fetched {:>7} total {:>7} skipped {:?} error {:?}",
                        mb["path"].as_str().unwrap_or("?"),
                        f,
                        mb["total"].as_i64().unwrap_or(0),
                        mb["skipped"].as_str(),
                        err
                    );
                }
                println!(
                    "  KPI messages/sec end-to-end: {:.0} ({} messages), report JSON {} bytes",
                    fetched as f64 / wall.as_secs_f64(),
                    fetched,
                    json.len()
                );
            }
            Err(e) => println!("  sync FAILED: {e:?}"),
        }
        let imap_us: u64 = timings
            .samples
            .iter()
            .filter(|(k, _)| k.starts_with("imap."))
            .map(|(_, v)| v.iter().sum::<u64>())
            .sum();
        let db_us: u64 = timings
            .samples
            .iter()
            .filter(|(k, _)| k.starts_with("db.") || k.starts_with("core."))
            .map(|(_, v)| v.iter().sum::<u64>())
            .sum();
        let wall_us = wall.as_micros() as u64;
        println!(
            "  split: imap (network+parse) {} ms ({:.0}%), db/core {} ms ({:.0}%), other {} ms",
            ms(imap_us),
            100.0 * imap_us as f64 / wall_us as f64,
            ms(db_us),
            100.0 * db_us as f64 / wall_us as f64,
            ms(wall_us.saturating_sub(imap_us + db_us))
        );
        print_phase_table(&timings);
        if let Some(mut lat) = reader_lat {
            lat.sort_unstable();
            println!(
                "  concurrent UI read (500-row list + counts): n {} p50 {} ms p95 {} ms max {} ms",
                lat.len(),
                ms(pct(&lat, 0.5)),
                ms(pct(&lat, 0.95)),
                ms(*lat.last().unwrap_or(&0))
            );
        }
        println!(
            "  profile on disk: {:.1} MiB",
            dir_size(dir.path()) as f64 / 1_048_576.0
        );
    }

    if let Some(mb) = env("BENCH_WIRE_PROBE") {
        println!();
        if let Err(e) = wire_probe(&tg, &mb) {
            println!("wire probe failed: {e}");
        }
    }
}
