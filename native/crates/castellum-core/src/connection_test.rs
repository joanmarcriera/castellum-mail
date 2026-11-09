// Castellum - account-setup connection preflight.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// "Test Connection" in the Add-account sheet: verify the IMAP and SMTP
// credentials BEFORE anything is persisted, and hand back the server's folder
// list so the user can choose what the first sync covers. Nothing here
// touches the database — no account row, no diagnostic row, no sync cursor —
// so a failed (or abandoned) test leaves no trace, which is the whole point
// (native/TASKS.md: "a 'Test Connection' step that shows this legible error
// before saving anything").
//
// Connection logic is NOT duplicated: IMAP goes through the same
// `imap_connect::connect_guarded_with` factory sync and replay use (so a
// panic in LOGIN/CAPABILITY/LIST is caught by `PanicGuardedProvider`), and
// SMTP through the same `LettreSmtpSender` send uses. Folder defaults come
// from `sync::should_sync_by_default`, the exact policy the first sync
// applies when no explicit mailbox filter is given — so "leave the defaults
// ticked" in the UI and "no filter" in the core always mean the same set.
//
// Errors are reported per leg as the redacted (kind, summary) pair from
// `redact::redact_provider_error` — never raw server text — so the Swift
// side maps `error_kind` with the same wording `AccountSyncStatus` uses.

use castellum_provider_imap::ImapProvider;
use castellum_provider_smtp::LettreSmtpSender;
use castellum_providers::{ImapConfig, MailProvider, ProviderError, SmtpConfig};
use serde::{Deserialize, Serialize};

use crate::imap_connect::connect_guarded_with;
use crate::redact;
use crate::send::SmtpSettings;
use crate::sync::{canonical_role_paths, effective_role, should_sync_by_default};
use crate::CoreError;

/// Request shape for `castellum_test_connection`. The IMAP fields match the
/// flat stored-account shape (`ImapSyncConfig`), so Swift can reuse its
/// encoder; `smtp` is the nested `SmtpSettings` shape `send` already takes,
/// with Swift's `effectiveSMTP` defaults applied. `smtp` absent = IMAP only.
///
/// `Debug` is manual so a stray `{:?}` never prints the password.
#[derive(Deserialize)]
pub struct ConnectionTestConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    #[serde(default = "default_true")]
    pub use_tls: bool,
    /// IMAP security mode, "tls" | "starttls" | "none". Absent
    /// = the legacy `use_tls` decides; see `TransportSecurity::resolve`.
    #[serde(default)]
    pub imap_security: Option<String>,
    #[serde(default)]
    pub allow_invalid_certs: bool,
    #[serde(default)]
    pub smtp: Option<SmtpSettings>,
}

impl std::fmt::Debug for ConnectionTestConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionTestConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("use_tls", &self.use_tls)
            .field("imap_security", &self.imap_security)
            .field("allow_invalid_certs", &self.allow_invalid_certs)
            .field("smtp", &self.smtp)
            .finish()
    }
}

fn default_true() -> bool {
    true
}

/// Outcome of one leg (IMAP or SMTP). `error_kind` is a
/// `redact::ErrorKind::as_str()` tag; `summary` its fixed, server-text-free
/// phrase. Both `None` on success.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LegResult {
    pub ok: bool,
    pub error_kind: Option<String>,
    pub summary: Option<String>,
}

impl LegResult {
    fn ok() -> Self {
        LegResult {
            ok: true,
            error_kind: None,
            summary: None,
        }
    }

    fn failed(e: &ProviderError) -> Self {
        let (kind, summary) = redact::redact_provider_error(e);
        LegResult {
            ok: false,
            error_kind: Some(kind.as_str().to_string()),
            summary: Some(summary.to_string()),
        }
    }
}

/// One server folder, as offered in the setup sheet's folder picker.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MailboxChoice {
    pub path: String,
    pub name: String,
    pub role: Option<String>,
    /// `\Noselect` containers can't hold mail; the UI lists them disabled.
    pub selectable: bool,
    /// What the first sync would cover with no explicit filter.
    pub sync_by_default: bool,
}

/// Full preflight result. The call itself only fails for a malformed
/// request; a connection failure is data (`imap.ok == false`), so the UI can
/// show IMAP and SMTP results side by side.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ConnectionTestReport {
    pub imap: LegResult,
    /// `None` when the request carried no `smtp` settings.
    pub smtp: Option<LegResult>,
    /// Empty unless the IMAP leg succeeded (LIST included).
    pub mailboxes: Vec<MailboxChoice>,
}

/// Run the preflight against real servers. Blocking network I/O — callers
/// must run it off the main thread. Never touches the profile database.
pub fn test_connection_json(config_json: &str) -> Result<String, CoreError> {
    let cfg: ConnectionTestConfig = serde_json::from_str(config_json)
        .map_err(|e| CoreError::Sync(format!("bad config json: {e}")))?;
    let report = test_connection_with(&cfg, ImapProvider::connect, |smtp| {
        LettreSmtpSender::new(smtp)?.test_connection()
    });
    serde_json::to_string(&report).map_err(|e| CoreError::Sync(format!("serialize report: {e}")))
}

/// `test_connection_json` with both connectors injected, so tests can drive
/// every failure path (including a panicking IMAP server) without a network.
fn test_connection_with<P: MailProvider>(
    cfg: &ConnectionTestConfig,
    imap_connect: impl FnOnce(&ImapConfig) -> Result<P, ProviderError>,
    smtp_test: impl FnOnce(&SmtpConfig) -> Result<(), ProviderError>,
) -> ConnectionTestReport {
    let imap_cfg = ImapConfig {
        host: cfg.host.clone(),
        port: cfg.port,
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        security: castellum_providers::TransportSecurity::resolve(
            cfg.imap_security.as_deref(),
            cfg.use_tls,
        ),
        allow_invalid_certs: cfg.allow_invalid_certs,
    };
    let (imap, mailboxes) = match connect_guarded_with(imap_connect, &imap_cfg) {
        Ok(mut provider) => {
            // LIST is part of the test: a server that logs in but can't list
            // folders can't sync either, and the folder picker needs it.
            let listed = provider.list_mailboxes();
            let _ = provider.logout();
            match listed {
                Ok(remote) => {
                    let canonical = canonical_role_paths(&remote);
                    let choices = remote
                        .iter()
                        .map(|rmb| {
                            let role = effective_role(rmb, &canonical);
                            MailboxChoice {
                                path: rmb.path.clone(),
                                name: rmb.name.clone(),
                                sync_by_default: should_sync_by_default(rmb, role.as_deref()),
                                role,
                                selectable: rmb.selectable,
                            }
                        })
                        .collect();
                    (LegResult::ok(), choices)
                }
                Err(e) => (LegResult::failed(&e), Vec::new()),
            }
        }
        Err(e) => (LegResult::failed(&e), Vec::new()),
    };

    // SMTP is tested even when IMAP failed: the user fixes both in one pass
    // instead of discovering the second problem only after the first.
    let smtp = cfg.smtp.as_ref().map(|s| {
        let smtp_cfg = SmtpConfig {
            host: s.host.clone(),
            port: s.port,
            username: s.username.clone(),
            password: s.password.clone(),
            use_tls: s.use_tls,
            allow_invalid_certs: s.allow_invalid_certs,
        };
        match smtp_test(&smtp_cfg) {
            Ok(()) => LegResult::ok(),
            Err(e) => LegResult::failed(&e),
        }
    });

    ConnectionTestReport {
        imap,
        smtp,
        mailboxes,
    }
}

#[cfg(test)]
mod tests {

    ///: the stored-account key resolves as the spec's table
    /// says, for every config shape that opens an IMAP connection.
    #[test]
    fn imap_security_key_resolves_for_every_imap_config_shape() {
        use castellum_providers::TransportSecurity::{self, *};
        let cases: [(&str, TransportSecurity); 5] = [
            (r#""use_tls":true"#, ImplicitTls),
            (r#""use_tls":false"#, Plaintext),
            (r#""use_tls":false,"imap_security":"starttls""#, StartTls),
            (r#""use_tls":true,"imap_security":"none""#, Plaintext),
            (r#""use_tls":false,"imap_security":"bogus""#, ImplicitTls),
        ];
        for (extra, want) in cases {
            let json = format!(
                r#"{{"account_id":"a","email":"a@x","host":"h","port":143,"username":"u","password":"p",{extra}}}"#
            );
            let sync: crate::sync::ImapSyncConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(
                TransportSecurity::resolve(sync.imap_security.as_deref(), sync.use_tls),
                want,
                "{extra}"
            );
            let replay: crate::replay::ReplayConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(
                TransportSecurity::resolve(replay.imap_security.as_deref(), replay.use_tls),
                want,
                "{extra}"
            );
            let test: ConnectionTestConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(
                TransportSecurity::resolve(test.imap_security.as_deref(), test.use_tls),
                want,
                "{extra}"
            );
        }
    }
    use super::*;
    use castellum_providers::{MailboxState, RemoteMailbox, RemoteMessage};

    struct FakeImap {
        mailboxes: Vec<RemoteMailbox>,
        list_panics: bool,
    }

    impl MailProvider for FakeImap {
        fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
            if self.list_panics {
                panic!("imap-proto parse failure on hostile LIST bytes SENTINEL");
            }
            Ok(self.mailboxes.clone())
        }
        fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
            unreachable!("a connection test must never SELECT")
        }
        fn fetch_new(&mut self, _after: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
            unreachable!("a connection test must never FETCH")
        }
        fn logout(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn mailbox(
        path: &str,
        role: Option<&str>,
        subscribed: bool,
        selectable: bool,
    ) -> RemoteMailbox {
        RemoteMailbox {
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            role: role.map(str::to_string),
            role_source: None,
            delimiter: Some("/".into()),
            subscribed,
            selectable,
        }
    }

    fn config(with_smtp: bool) -> ConnectionTestConfig {
        let json = if with_smtp {
            r#"{"host":"imap.example.com","port":993,"username":"a@example.com",
                "password":"pw","smtp":{"host":"smtp.example.com","port":587,
                "username":"a@example.com","password":"pw"}}"#
        } else {
            r#"{"host":"imap.example.com","port":993,"username":"a@example.com","password":"pw"}"#
        };
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn success_lists_folders_with_the_first_sync_default_policy() {
        let report = test_connection_with(
            &config(true),
            |_| {
                Ok(FakeImap {
                    mailboxes: vec![
                        mailbox("INBOX", Some("inbox"), false, true),
                        mailbox("[Gmail]/All Mail", Some("archive"), false, true),
                        mailbox("[Gmail]", None, false, false),
                        mailbox("Projects", None, true, true),
                        mailbox("Old", None, false, true),
                    ],
                    list_panics: false,
                })
            },
            |_| Ok(()),
        );
        assert_eq!(report.imap, LegResult::ok());
        assert_eq!(report.smtp, Some(LegResult::ok()));
        let defaults: Vec<(&str, bool, bool)> = report
            .mailboxes
            .iter()
            .map(|m| (m.path.as_str(), m.selectable, m.sync_by_default))
            .collect();
        assert_eq!(
            defaults,
            vec![
                ("INBOX", true, true),
                // Archive/All Mail waits for an explicit opt-in, exactly as
                // the unfiltered first sync treats it.
                ("[Gmail]/All Mail", true, false),
                ("[Gmail]", false, false),
                ("Projects", true, true),
                ("Old", true, false),
            ]
        );
    }

    #[test]
    fn imap_auth_failure_is_redacted_and_smtp_is_still_tested() {
        let mut smtp_ran = false;
        let report = test_connection_with(
            &config(true),
            |_| -> Result<FakeImap, _> {
                Err(ProviderError::Auth(
                    "NO [AUTHENTICATIONFAILED] user=a pass=pw SENTINEL".into(),
                ))
            },
            |_| {
                smtp_ran = true;
                Err(ProviderError::Network("connection refused SENTINEL".into()))
            },
        );
        assert!(smtp_ran, "SMTP must be tested even when IMAP failed");
        assert_eq!(report.imap.error_kind.as_deref(), Some("auth"));
        assert_eq!(
            report.smtp.as_ref().unwrap().error_kind.as_deref(),
            Some("network")
        );
        assert!(report.mailboxes.is_empty());
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            !json.contains("SENTINEL"),
            "server text leaked into the report: {json}"
        );
    }

    #[test]
    fn a_parser_panic_during_list_becomes_a_protocol_error() {
        let report = test_connection_with(
            &config(false),
            |_| {
                Ok(FakeImap {
                    mailboxes: vec![],
                    list_panics: true,
                })
            },
            |_| unreachable!("no smtp settings were given"),
        );
        assert_eq!(report.imap.error_kind.as_deref(), Some("protocol"));
        assert_eq!(report.smtp, None);
        assert!(!serde_json::to_string(&report).unwrap().contains("SENTINEL"));
    }

    #[test]
    fn a_panic_during_connect_is_caught_too() {
        let report = test_connection_with(
            &config(false),
            |_| -> Result<FakeImap, ProviderError> { panic!("LOGIN parse SENTINEL") },
            |_| Ok(()),
        );
        assert!(!report.imap.ok);
        assert_eq!(report.imap.error_kind.as_deref(), Some("protocol"));
    }

    #[test]
    fn malformed_request_is_an_error_not_a_report() {
        assert!(test_connection_json("{\"host\":1}").is_err());
    }

    #[test]
    fn debug_never_prints_either_password() {
        let dbg = format!("{:?}", config(true));
        assert!(!dbg.contains("\"pw\""), "{dbg}");
    }
}
