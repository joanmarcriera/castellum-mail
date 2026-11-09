// Castellum - IMAP connect/read timeout test (FP28).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A server that accepts the TCP connection and then says nothing must make
// `ImapProvider::connect_with_timeouts` fail with `ProviderError::Network`
// within the injected bound, instead of blocking the sync thread forever.
// Local listener only; no real server is contacted.

use castellum_provider_imap::ImapProvider;
use castellum_providers::{ImapConfig, ProviderError};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[test]
fn silent_server_times_out_with_a_network_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Accept and hold the socket open, never writing a greeting.
    let holder = std::thread::spawn(move || {
        let conn = listener.accept().map(|(s, _)| s);
        std::thread::sleep(Duration::from_secs(3));
        drop(conn);
    });

    let cfg = ImapConfig {
        host: "127.0.0.1".into(),
        port,
        username: "u".into(),
        password: "p".into(),
        security: castellum_providers::TransportSecurity::Plaintext,
        allow_invalid_certs: false,
    };
    let started = Instant::now();
    let result = ImapProvider::connect_with_timeouts(
        &cfg,
        Duration::from_secs(2),
        Duration::from_millis(300),
    );
    let elapsed = started.elapsed();
    match result {
        Err(ProviderError::Network(msg)) => {
            assert!(msg.contains("timed out"), "readable message, got {msg:?}");
        }
        Err(other) => panic!("expected Network error, got {other:?}"),
        Ok(_) => panic!("a silent server must not connect"),
    }
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    holder.join().unwrap();
}
