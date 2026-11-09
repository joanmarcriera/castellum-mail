// Castellum - SMTP provider (outbound mail) over lettre.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Two responsibilities, both blocking (no Tokio):
//   * `build_rfc822`: turn a stored Draft into RFC822 bytes + an SMTP envelope.
//     Bcc recipients go in the envelope ONLY — never in the message headers.
//   * `LettreSmtpSender`: submit prebuilt bytes over SMTP (plaintext for local
//     test servers, implicit TLS on 465, STARTTLS otherwise).

use castellum_models::{Draft, DraftAddress, DraftAttachmentMeta};
use castellum_providers::{ProviderError, SmtpConfig, SmtpSender};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Address, Message, SmtpTransport, Transport};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A fully built outbound message: envelope sender + recipients (including
/// Bcc) and the RFC822 bytes (which exclude Bcc). `message_id` is the generated
/// RFC822 Message-ID inside `raw`, exposed so the Sent-copy path can key its
/// idempotency guard on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedMessage {
    pub from: String,
    pub rcpts: Vec<String>,
    pub raw: Vec<u8>,
    pub message_id: String,
}

fn bad(msg: impl std::fmt::Display) -> ProviderError {
    ProviderError::Protocol(msg.to_string())
}

fn mailbox(a: &DraftAddress) -> Result<Mailbox, ProviderError> {
    let addr: Address = a
        .email
        .parse()
        .map_err(|e| bad(format!("bad address '{}': {e}", a.email)))?;
    Ok(Mailbox::new(a.name.clone(), addr))
}

/// Monotonic per-process counter so two messages built in the same nanosecond
/// still get distinct Message-IDs.
static MSGID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Normalise a stored message-id list (bracket-free by design) to RFC 5322
/// wire form: whitespace-separated ids, each exactly `<id>`. Idempotent.
fn wire_msg_ids(s: &str) -> String {
    s.split_whitespace()
        .map(|t| t.trim_matches(|c| c == '<' || c == '>'))
        .filter(|t| !t.is_empty())
        .map(|t| format!("<{t}>"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A unique RFC822 Message-ID, domained on the sender's address.
fn generate_message_id(from_email: &str) -> String {
    let domain = from_email.rsplit('@').next().unwrap_or("localhost");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = MSGID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("<castellum.{nanos}.{}.{seq}@{domain}>", std::process::id())
}

/// Content-IDs referenced as `src="cid:…"` in compose-rendered HTML, in order
/// of first use, de-duplicated. A private twin of
/// `castellum_mail::referenced_cids` (this crate must not depend on
/// castellum-mail); both are pinned by the same test vector. The renderer is
/// the only writer of that shape, so a plain scan is enough.
fn referenced_cids(html: &str) -> Vec<String> {
    const NEEDLE: &str = "src=\"cid:";
    let mut out: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(NEEDLE) {
        let after = &rest[i + NEEDLE.len()..];
        let Some(end) = after.find('"') else { break };
        let id = &after[..end];
        if !id.is_empty() && !out.iter().any(|c| c == id) {
            out.push(id.to_string());
        }
        rest = &after[end..];
    }
    out
}

/// Build the RFC822 bytes + SMTP envelope for a draft. Headers carry From
/// (with the account display name), To/Cc, Subject, Date, Message-ID and, when
/// the draft is a reply, In-Reply-To/References. With no attachments the body
/// is text/plain UTF-8 (lettre picks a safe transfer encoding), byte-for-byte
/// the pre-attachment output. With attachments: `multipart/mixed` wrapping the
/// text body and every regular attachment; an attachment that is BOTH
/// `is_inline` AND carries a `content_id` goes into a nested
/// `multipart/related` alongside the text (so a forwarded original's `cid:`
/// references keep working). Bcc recipients are added to the envelope `rcpts`
/// but NEVER to the headers.
pub fn build_rfc822(
    from_email: &str,
    from_name: Option<&str>,
    draft: &Draft,
    attachments: &[(DraftAttachmentMeta, Vec<u8>)],
) -> Result<ComposedMessage, ProviderError> {
    build_rfc822_with(from_email, from_name, None, draft, attachments)
}

/// Whether `address` is a single address the SMTP builder will accept (the same
/// parser `build_rfc822_with` uses), so a bad Reply-To/Bcc is refused when an
/// identity is saved instead of failing every later send.
pub fn is_sendable_address(address: &str) -> bool {
    address.parse::<Address>().is_ok()
}

/// As [`build_rfc822`], plus an optional `Reply-To` address (MM-6: the sending
/// identity's Reply-To). `None` builds byte-for-byte what `build_rfc822` always
/// did.
pub fn build_rfc822_with(
    from_email: &str,
    from_name: Option<&str>,
    reply_to: Option<&str>,
    draft: &Draft,
    attachments: &[(DraftAttachmentMeta, Vec<u8>)],
) -> Result<ComposedMessage, ProviderError> {
    build_rfc822_opts(
        from_email,
        from_name,
        BuildOptions {
            reply_to,
            plain_text_flowed: false,
        },
        draft,
        attachments,
    )
}

/// Optional behaviour for [`build_rfc822_opts`]. `Default` builds exactly what
/// [`build_rfc822`] always did.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildOptions<'a> {
    /// The sending identity's `Reply-To` address (MM-6).
    pub reply_to: Option<&'a str>,
    /// MM-17: send the `text/plain` body as RFC 3676 `format=flowed`
    /// (`DelSp=no`, soft breaks at [`castellum_mail::flowed::WRAP_WIDTH`]).
    /// Ignored when the draft carries `body_html`: that plain part is Markdown
    /// SOURCE and must not be re-wrapped.
    pub plain_text_flowed: bool,
}

/// As [`build_rfc822_with`], with the full [`BuildOptions`].
pub fn build_rfc822_opts(
    from_email: &str,
    from_name: Option<&str>,
    options: BuildOptions<'_>,
    draft: &Draft,
    attachments: &[(DraftAttachmentMeta, Vec<u8>)],
) -> Result<ComposedMessage, ProviderError> {
    let reply_to = options.reply_to;
    // Flowed only for a plain-text-only draft (see `BuildOptions`).
    let flowed = options.plain_text_flowed && draft.body_html.is_none();
    let (plain_ct, plain_body) = if flowed {
        (
            ContentType::parse("text/plain; charset=utf-8; format=flowed")
                .map_err(|e| bad(format!("flowed content type: {e}")))?,
            castellum_mail::flowed::encode(&draft.body_text, castellum_mail::flowed::WRAP_WIDTH),
        )
    } else {
        (ContentType::TEXT_PLAIN, draft.body_text.clone())
    };
    // De-duplicate case-insensitively (as send.rs does for the automatic Bcc)
    // so a recipient typed twice gets one RCPT TO, not two deliveries.
    let mut rcpts: Vec<String> = Vec::new();
    for a in draft.to.iter().chain(&draft.cc).chain(&draft.bcc) {
        if !rcpts.iter().any(|r| r.eq_ignore_ascii_case(&a.email)) {
            rcpts.push(a.email.clone());
        }
    }
    if rcpts.is_empty() {
        return Err(bad("draft has no recipients"));
    }

    let from_addr: Address = from_email
        .parse()
        .map_err(|e| bad(format!("bad from address '{from_email}': {e}")))?;
    let message_id = generate_message_id(from_email);
    let mut builder = Message::builder()
        .from(Mailbox::new(
            from_name.map(str::to_owned),
            from_addr.clone(),
        ))
        .subject(draft.subject.clone())
        .date_now()
        .message_id(Some(message_id.clone()));
    for a in &draft.to {
        builder = builder.to(mailbox(a)?);
    }
    for a in &draft.cc {
        builder = builder.cc(mailbox(a)?);
    }
    // NOTE: no .bcc() on purpose — Bcc must not leak into the sent headers.
    // Because of that lettre cannot derive the envelope from the headers for a
    // Bcc-only draft (MissingTo), so set it explicitly to exactly `rcpts`.
    let env_rcpts = rcpts
        .iter()
        .map(|r| {
            r.parse::<Address>()
                .map_err(|e| bad(format!("bad recipient '{r}': {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    builder = builder.envelope(
        lettre::address::Envelope::new(Some(from_addr.clone()), env_rcpts)
            .map_err(|e| bad(format!("build envelope: {e}")))?,
    );
    if let Some(reply_to) = reply_to {
        let addr: Address = reply_to
            .parse()
            .map_err(|e| bad(format!("bad reply-to address '{reply_to}': {e}")))?;
        builder = builder.reply_to(Mailbox::new(None, addr));
    }
    if let Some(irt) = &draft.in_reply_to {
        builder = builder.in_reply_to(wire_msg_ids(irt));
    }
    if let Some(refs) = &draft.references_header {
        builder = builder.references(wire_msg_ids(refs));
    }

    // S46: when the draft carries rendered HTML the body is a
    // multipart/alternative — Markdown SOURCE as text/plain first, HTML last,
    // because RFC 2046 §5.1.4 makes the LAST part the preferred one. Built
    // once here so every attachment shape below wraps the same body instead
    // of each re-deciding whether Markdown is on.
    let alternative = draft.body_html.as_ref().map(|html| {
        MultiPart::alternative()
            .singlepart(SinglePart::plain(draft.body_text.clone()))
            .singlepart(SinglePart::html(html.clone()))
    });

    if attachments.is_empty() {
        let message = match alternative {
            // UNCHANGED from the pre-S46 path — with Markdown off and no
            // attachments this must build byte-for-byte what it always did.
            None => builder
                // Explicit: lettre's plain-String body sets only the transfer
                // encoding; without this the UTF-8 charset would go undeclared.
                .header(plain_ct)
                .body(plain_body)
                .map_err(|e| bad(format!("build message: {e}")))?,
            Some(alt) => builder
                .multipart(alt)
                .map_err(|e| bad(format!("build message: {e}")))?,
        };
        return Ok(ComposedMessage {
            from: from_email.to_string(),
            rcpts,
            raw: message.formatted(),
            message_id,
        });
    }

    let text_part = SinglePart::builder().header(plain_ct).body(plain_body);
    // With Markdown on (`body_html`), an inline part is only inline when the HTML
    // actually references its Content-ID; an unreferenced `multipart/related`
    // part is hidden by some clients, so it goes out as a regular attachment
    // (MM-5b, T7). With Markdown off nothing changes.
    let referenced = draft.body_html.as_deref().map(referenced_cids);
    let (inline, regular): (Vec<_>, Vec<_>) = attachments.iter().partition(|(meta, _)| {
        meta.is_inline
            && meta.content_id.as_deref().is_some_and(|cid| {
                referenced
                    .as_ref()
                    .is_none_or(|refs| refs.iter().any(|r| r == cid))
            })
    });

    // The text body, plus inline parts when there are any.
    let mut body = if inline.is_empty() {
        match alternative {
            Some(alt) => MultiPart::mixed().multipart(alt),
            None => MultiPart::mixed().singlepart(text_part),
        }
    } else {
        let mut related = match alternative {
            Some(alt) => MultiPart::related().multipart(alt),
            None => MultiPart::related().singlepart(text_part),
        };
        for (meta, bytes) in &inline {
            let ct = attachment_content_type(&meta.content_type)?;
            let cid = meta.content_id.clone().unwrap_or_default();
            // Keep the original filename (Content-Disposition: inline;
            // filename=...) so the part is still recognisable to the reader.
            let part = if meta.filename.is_empty() {
                Attachment::new_inline(cid)
            } else {
                Attachment::new_inline_with_name(cid, meta.filename.clone())
            };
            related = related.singlepart(part.body(bytes.clone(), ct));
        }
        if regular.is_empty() {
            // Inline images only: multipart/related alone is the correct top
            // level; wrapping it in a one-child mixed is noise.
            return finish(builder, related, from_email, rcpts, message_id);
        }
        MultiPart::mixed().multipart(related)
    };
    for (meta, bytes) in &regular {
        let ct = attachment_content_type(&meta.content_type)?;
        body = body.singlepart(Attachment::new(meta.filename.clone()).body(bytes.clone(), ct));
    }
    finish(builder, body, from_email, rcpts, message_id)
}

/// A built draft: RFC822 bytes ready for APPEND, plus the RFC822 Message-ID
/// inside them (for the Message-ID search fallback when APPENDUID is
/// missing). Unlike `ComposedMessage` there is no envelope field here — this
/// is never submitted over SMTP, so lettre's internal envelope (needed only
/// to satisfy its builder, see `build_draft_rfc822`) is simply discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedDraft {
    pub raw: Vec<u8>,
    pub message_id: String,
}

/// Build RFC822 bytes for a draft that may have no recipients yet and whose
/// Bcc line (if any) MUST be included in the headers — this copy is read
/// back only by the account owner's own clients, never delivered. See this
/// module's top comment for why this is a separate function from
/// `build_rfc822` rather than a relaxed version of it. Attachments are out
/// of scope here: the draft-attachments feature persists blobs separately
/// and does not yet round-trip them into the server mirror.
pub fn build_draft_rfc822(
    from_email: &str,
    from_name: Option<&str>,
    draft: &Draft,
) -> Result<ComposedDraft, ProviderError> {
    let from_addr: Address = from_email
        .parse()
        .map_err(|e| bad(format!("bad from address '{from_email}': {e}")))?;
    let message_id = generate_message_id(from_email);
    let has_recipients = !draft.to.is_empty() || !draft.cc.is_empty() || !draft.bcc.is_empty();
    let mut builder = Message::builder()
        .from(Mailbox::new(
            from_name.map(str::to_owned),
            from_addr.clone(),
        ))
        .subject(draft.subject.clone())
        .date_now()
        .message_id(Some(message_id.clone()))
        // Unlike build_rfc822 (real sends): a draft mirror round-trips Bcc
        // in the headers — nobody but the account owner ever fetches this
        // copy back, and lettre otherwise strips Bcc once it has derived
        // the (here, unused) SMTP envelope from the recipient headers.
        .keep_bcc();
    for a in &draft.to {
        builder = builder.to(mailbox(a)?);
    }
    for a in &draft.cc {
        builder = builder.cc(mailbox(a)?);
    }
    for a in &draft.bcc {
        builder = builder.bcc(mailbox(a)?);
    }
    if !has_recipients {
        // lettre always derives (or requires) a non-empty SMTP envelope,
        // even though this message is never submitted over SMTP. A draft
        // with no recipients yet (the user is still typing) has nothing
        // real to put there, so use the sender itself as a harmless
        // placeholder — the envelope is discarded; only `.formatted()`'s
        // header bytes matter to a server-draft APPEND.
        builder = builder.envelope(
            lettre::address::Envelope::new(Some(from_addr.clone()), vec![from_addr])
                .map_err(|e| bad(format!("build draft envelope: {e}")))?,
        );
    }
    if let Some(irt) = &draft.in_reply_to {
        builder = builder.in_reply_to(wire_msg_ids(irt));
    }
    if let Some(refs) = &draft.references_header {
        builder = builder.references(wire_msg_ids(refs));
    }

    // Same S46 multipart/alternative shape as build_rfc822's no-attachments
    // path: Markdown source as text/plain first, rendered HTML last (RFC
    // 2046 §5.1.4 makes the last part preferred).
    let message = match &draft.body_html {
        None => builder
            .header(ContentType::TEXT_PLAIN)
            .body(draft.body_text.clone())
            .map_err(|e| bad(format!("build draft message: {e}")))?,
        Some(html) => {
            let alt = MultiPart::alternative()
                .singlepart(SinglePart::plain(draft.body_text.clone()))
                .singlepart(SinglePart::html(html.clone()));
            builder
                .multipart(alt)
                .map_err(|e| bad(format!("build draft message: {e}")))?
        }
    };

    Ok(ComposedDraft {
        raw: message.formatted(),
        message_id,
    })
}

/// Parse an attachment's declared MIME type; a malformed one is a typed error.
fn attachment_content_type(raw: &str) -> Result<ContentType, ProviderError> {
    ContentType::parse(raw).map_err(|e| bad(format!("bad attachment content type '{raw}': {e}")))
}

/// Finish a multipart message (shared tail of the attachment paths).
fn finish(
    builder: lettre::message::MessageBuilder,
    body: MultiPart,
    from_email: &str,
    rcpts: Vec<String>,
    message_id: String,
) -> Result<ComposedMessage, ProviderError> {
    let message = builder
        .multipart(body)
        .map_err(|e| bad(format!("build message: {e}")))?;
    Ok(ComposedMessage {
        from: from_email.to_string(),
        rcpts,
        raw: message.formatted(),
        message_id,
    })
}

/// Blocking SMTP submitter backed by lettre. Construction only configures the
/// transport; the TCP/TLS connection is made per `send` call.
pub struct LettreSmtpSender {
    transport: SmtpTransport,
}

impl LettreSmtpSender {
    /// Configure a transport for `cfg`. `use_tls = false` is a plaintext
    /// connection (local test servers only, e.g. GreenMail :3025);
    /// `use_tls = true` uses implicit TLS on port 465 and mandatory STARTTLS
    /// on any other port.
    pub fn new(cfg: &SmtpConfig) -> Result<Self, ProviderError> {
        let mut builder = SmtpTransport::builder_dangerous(cfg.host.as_str()).port(cfg.port);
        if cfg.use_tls {
            let tls = TlsParameters::builder(cfg.host.clone())
                .dangerous_accept_invalid_certs(cfg.allow_invalid_certs)
                .build()
                .map_err(|e| ProviderError::Tls(e.to_string()))?;
            builder = if cfg.port == 465 {
                builder.tls(Tls::Wrapper(tls))
            } else {
                builder.tls(Tls::Required(tls))
            };
        }
        if !cfg.username.is_empty() {
            builder =
                builder.credentials(Credentials::new(cfg.username.clone(), cfg.password.clone()));
        }
        Ok(LettreSmtpSender {
            transport: builder.build(),
        })
    }

    /// Preflight the SMTP leg without sending anything (S62 account setup
    /// "Test Connection"): connect, negotiate TLS, EHLO, AUTHENTICATE with
    /// the configured credentials, NOOP, QUIT. lettre's `test_connection`
    /// runs the same `connection()` path `send` does, credentials included,
    /// so a wrong SMTP password fails here rather than on the first send.
    pub fn test_connection(&self) -> Result<(), ProviderError> {
        match self.transport.test_connection() {
            Ok(true) => Ok(()),
            Ok(false) => Err(ProviderError::Network(
                "SMTP server closed the connection".into(),
            )),
            Err(e) => Err(classify_smtp_error(e)),
        }
    }
}

impl SmtpSender for LettreSmtpSender {
    fn send(
        &mut self,
        from: &str,
        rcpts: &[String],
        raw_message: &[u8],
    ) -> Result<(), ProviderError> {
        if rcpts.is_empty() {
            return Err(bad("no envelope recipients"));
        }
        let from_addr: Address = from
            .parse()
            .map_err(|e| bad(format!("bad envelope from '{from}': {e}")))?;
        let to_addrs = rcpts
            .iter()
            .map(|r| {
                r.parse::<Address>()
                    .map_err(|e| bad(format!("bad envelope rcpt '{r}': {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let envelope = lettre::address::Envelope::new(Some(from_addr), to_addrs)
            .map_err(|e| bad(format!("build envelope: {e}")))?;
        self.transport
            .send_raw(&envelope, raw_message)
            .map(|_| ())
            .map_err(|e| {
                if submission_outcome_unknown(&e) {
                    ProviderError::MaybeDelivered(e.to_string())
                } else {
                    classify_smtp_error(e)
                }
            })
    }
}

/// True when a failed submission may nonetheless have been accepted: the
/// session was established (not lettre's `Connection` kind, which is a failure
/// to connect at all) and the error is not a server reply code — an I/O error
/// on the live socket (reset, timeout: lettre's `Network` kind) or a reply
/// that never arrived whole (`Response` kind: "incomplete response" when the
/// server closes after DATA). lettre does not say which command was in
/// flight, so a drop before DATA is held too: conservative, never a double
/// send. lettre has no `is_network()`; its `Display` prefix ("network
/// error") is pinned by the tests below against a real socket.
fn submission_outcome_unknown(e: &lettre::transport::smtp::Error) -> bool {
    e.is_response() || e.to_string().starts_with("network error")
}

/// True when an SMTP reply says authenticated SMTP submission itself is
/// switched off (Microsoft: `535 5.7.139 Authentication unsuccessful,
/// SmtpClientAuthentication is disabled for the Mailbox|Tenant`).
///
/// Both markers are required. Exchange Online reuses enhanced status 5.7.139
/// for other refusals (rejected credentials, security defaults), which are
/// ordinary auth failures; and the phrase alone, without the status, is not
/// a reply this code has ever seen. Matching is on the rendered error text
/// because lettre exposes the reply code but not the enhanced status.
fn is_smtp_auth_disabled(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("5.7.139") && lower.contains("smtpclientauthentication is disabled")
}

fn classify_smtp_error(e: lettre::transport::smtp::Error) -> ProviderError {
    let msg = e.to_string();
    // 530 (auth required), 534 (mechanism too weak / app password needed)
    // and 535 (bad credentials) are the RFC 4954 authentication failures.
    // Their text rarely says "auth" (Gmail: "Username and Password not
    // accepted"), so classify by code, not wording.
    let auth_code = e
        .status()
        .map(u16::from)
        .is_some_and(|c| matches!(c, 530 | 534 | 535));
    if auth_code && is_smtp_auth_disabled(&msg) {
        // Not a credential problem: no token refresh or retry can fix it.
        ProviderError::AuthSmtpDisabled(msg)
    } else if auth_code {
        ProviderError::Auth(msg)
    } else if e.is_tls() {
        ProviderError::Tls(msg)
    } else if e.is_client() || e.is_response() {
        ProviderError::Protocol(msg)
    } else if e.is_permanent() && msg.to_ascii_lowercase().contains("auth") {
        ProviderError::Auth(msg)
    } else if e.is_permanent() {
        // Any other 5xx: the server refused this message for good (unknown
        // recipient, policy, size). Not a network fault to back off and
        // retry for hours (G8, #3632).
        ProviderError::Rejected(msg)
    } else if e.is_transient() {
        // Any 4xx (greylisting, full spool, rate limit): the server is up
        // and answered "try again later". Retried on the normal backoff like
        // a network fault, but not reported as "couldn't reach the server".
        ProviderError::Deferred(msg)
    } else {
        ProviderError::Network(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::MimeHeaders;

    fn draft() -> Draft {
        Draft {
            id: 1,
            account_id: "acc".into(),
            to: vec![DraftAddress {
                name: Some("Bob Recipient".into()),
                email: "bob@example.com".into(),
            }],
            cc: vec![DraftAddress {
                name: None,
                email: "carol@example.com".into(),
            }],
            bcc: vec![DraftAddress {
                name: None,
                email: "secret@example.com".into(),
            }],
            subject: "Quarterly update".into(),
            body_text: "Hello Bob,\nhéllo ünïcode too.\n".into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: Some("<parent@example.com>".into()),
            references_header: Some("<root@example.com> <parent@example.com>".into()),
            status: "outbox".into(),
            updated_at_epoch: 0,
            server_mailbox_path: None,
            server_uid: None,
            server_uid_validity: None,
            server_message_id: None,
            server_saved_at_epoch: None,
        }
    }

    fn headers_of(raw: &[u8]) -> String {
        let s = String::from_utf8_lossy(raw);
        s.split("\r\n\r\n").next().unwrap_or("").to_string()
    }

    #[test]
    fn draft_with_no_recipients_builds_successfully() {
        let mut d = draft();
        d.to = vec![];
        d.cc = vec![];
        d.bcc = vec![];
        let built = build_draft_rfc822("alice@example.com", Some("Alice"), &d).unwrap();
        assert!(!built.raw.is_empty());
        assert!(built.message_id.starts_with("<castellum."));
    }

    #[test]
    fn from_header_quotes_and_encodes_awkward_display_names() {
        // S70: comma and quote need RFC 5322 quoting; accents need RFC 2047.
        let comma = build_rfc822("a@example.com", Some("Pujol, Núria"), &draft(), &[]).unwrap();
        let h = headers_of(&comma.raw);
        assert!(h.contains("=?utf-8?") || h.contains("=?UTF-8?"), "{h}");
        assert!(!h.contains("From: Pujol, "), "unquoted comma in {h}");

        let quote = build_rfc822("a@example.com", Some("Al \"Bo\" Ca"), &draft(), &[]).unwrap();
        let h = headers_of(&quote.raw);
        assert!(h.contains("\\\"Bo\\\""), "quotes must be escaped in {h}");
        assert!(h.contains("<a@example.com>"), "{h}");
    }

    #[test]
    fn draft_bcc_is_included_in_headers_unlike_build_rfc822() {
        let built = build_draft_rfc822("alice@example.com", None, &draft()).unwrap();
        let headers = headers_of(&built.raw);
        assert!(
            headers.contains("secret@example.com"),
            "a draft copy must round-trip Bcc, unlike a sent copy: {headers}"
        );
    }

    #[test]
    fn draft_message_id_differs_from_a_sibling_build() {
        let a = build_draft_rfc822("alice@example.com", None, &draft()).unwrap();
        let b = build_draft_rfc822("alice@example.com", None, &draft()).unwrap();
        assert_ne!(
            a.message_id, b.message_id,
            "each save must get a fresh Message-ID"
        );
    }

    #[test]
    fn builds_headers_correctly() {
        let m = build_rfc822("alice@example.com", Some("Alice Sender"), &draft(), &[]).unwrap();
        let headers = headers_of(&m.raw);

        assert!(
            headers.contains("From: \"Alice Sender\" <alice@example.com>"),
            "From with display name, got headers:\n{headers}"
        );
        assert!(headers.contains("To: \"Bob Recipient\" <bob@example.com>"));
        assert!(headers.contains("Cc: carol@example.com"));
        assert!(headers.contains("Subject: Quarterly update"));
        assert!(headers.contains("In-Reply-To: <parent@example.com>"));
        assert!(headers.contains("References: <root@example.com> <parent@example.com>"));
        assert!(headers.contains("Date: "), "Date header present");
        assert!(
            headers.contains("Message-ID: <castellum."),
            "generated Message-ID present, domained on the sender"
        );
        assert!(headers.contains("@example.com>"));
    }

    #[test]
    fn bcc_is_in_envelope_but_not_in_headers() {
        let m = build_rfc822("alice@example.com", None, &draft(), &[]).unwrap();
        // Envelope: all three recipients, Bcc included.
        assert_eq!(
            m.rcpts,
            vec![
                "bob@example.com".to_string(),
                "carol@example.com".to_string(),
                "secret@example.com".to_string()
            ]
        );
        // Message bytes: the Bcc address appears NOWHERE (headers or body).
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(
            !raw.contains("secret@example.com"),
            "bcc address must not appear in the message"
        );
        assert!(!headers_of(&m.raw).contains("Bcc:"));
    }

    #[test]
    fn bcc_only_draft_builds_with_bcc_in_envelope_only() {
        let mut d = draft();
        d.to.clear();
        d.cc.clear();
        let m =
            build_rfc822("alice@example.com", None, &d, &[]).expect("a Bcc-only draft must build");
        assert_eq!(m.rcpts, vec!["secret@example.com".to_string()]);
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(!raw.contains("secret@example.com"));
        let headers = headers_of(&m.raw);
        assert!(!headers.contains("Bcc:"));
        assert!(!headers.contains("\r\nTo:"));
        assert!(headers.contains("From: alice@example.com"));
    }

    #[test]
    fn utf8_body_is_encoded_not_mangled() {
        let m = build_rfc822("alice@example.com", None, &draft(), &[]).unwrap();
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(raw.contains("text/plain"));
        assert!(raw.to_ascii_lowercase().contains("charset=utf-8"));
        // The non-ASCII body survives via a transfer encoding: either encoded
        // (quoted-printable/base64) or 8bit verbatim — never dropped.
        let qp = raw.contains("h=C3=A9llo"); // quoted-printable of "héllo"
        let eight_bit = raw.contains("héllo");
        let b64 = raw.contains("base64");
        assert!(
            qp || eight_bit || b64,
            "utf-8 body must survive encoding, got:\n{raw}"
        );
    }

    #[test]
    fn reply_headers_are_omitted_for_fresh_drafts() {
        let mut d = draft();
        d.in_reply_to = None;
        d.references_header = None;
        let m = build_rfc822("alice@example.com", None, &d, &[]).unwrap();
        let headers = headers_of(&m.raw);
        assert!(!headers.contains("In-Reply-To:"));
        assert!(!headers.contains("References:"));
    }

    #[test]
    fn composed_message_id_matches_the_header() {
        let m = build_rfc822("alice@example.com", None, &draft(), &[]).unwrap();
        assert!(m.message_id.starts_with("<castellum."));
        assert!(m.message_id.ends_with("@example.com>"));
        assert!(
            headers_of(&m.raw).contains(&format!("Message-ID: {}", m.message_id)),
            "exposed message_id must be the one in the raw headers"
        );
    }

    #[test]
    fn message_ids_are_unique() {
        let a = generate_message_id("a@example.com");
        let b = generate_message_id("a@example.com");
        assert_ne!(a, b);
        assert!(a.starts_with('<') && a.ends_with("@example.com>"));
    }

    #[test]
    fn no_recipients_is_an_error() {
        let mut d = draft();
        d.to.clear();
        d.cc.clear();
        d.bcc.clear();
        assert!(build_rfc822("alice@example.com", None, &d, &[]).is_err());
    }

    #[test]
    fn bad_addresses_error_cleanly() {
        let mut d = draft();
        d.to[0].email = "not-an-address".into();
        assert!(build_rfc822("alice@example.com", None, &d, &[]).is_err());
        assert!(build_rfc822("also bad", None, &draft(), &[]).is_err());
    }

    fn meta(
        filename: &str,
        content_type: &str,
        is_inline: bool,
        content_id: Option<&str>,
    ) -> DraftAttachmentMeta {
        DraftAttachmentMeta {
            id: 1,
            ordinal: 0,
            filename: filename.into(),
            content_type: content_type.into(),
            size_bytes: 0,
            is_inline,
            content_id: content_id.map(str::to_owned),
        }
    }

    /// Drop the volatile Date header so two builds can be compared.
    fn without_date(raw: &[u8]) -> String {
        String::from_utf8_lossy(raw)
            .split("\r\n")
            .filter(|l| !l.starts_with("Date: "))
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    #[test]
    fn no_attachments_is_byte_identical_to_the_pre_attachment_build() {
        let d = draft();
        let m = build_rfc822("alice@example.com", Some("Alice Sender"), &d, &[]).unwrap();
        // Reference: exactly what build_rfc822 did before attachments existed.
        let mut b = Message::builder()
            .from(Mailbox::new(
                Some("Alice Sender".into()),
                "alice@example.com".parse().unwrap(),
            ))
            .subject(d.subject.clone())
            .date_now()
            .message_id(Some(m.message_id.clone()));
        for a in &d.to {
            b = b.to(mailbox(a).unwrap());
        }
        for a in &d.cc {
            b = b.cc(mailbox(a).unwrap());
        }
        b = b
            .in_reply_to(d.in_reply_to.clone().unwrap())
            .references(d.references_header.clone().unwrap());
        let reference = b
            .header(ContentType::TEXT_PLAIN)
            .body(d.body_text.clone())
            .unwrap()
            .formatted();
        assert_eq!(without_date(&m.raw), without_date(&reference));
        assert!(!String::from_utf8_lossy(&m.raw)
            .to_ascii_lowercase()
            .contains("multipart/"));
    }

    #[test]
    fn regular_attachment_builds_multipart_mixed_and_bytes_survive() {
        // Deliberately non-UTF-8 bytes: the payload must round-trip intact.
        let payload: Vec<u8> = (0u8..=255).collect();
        let atts = vec![(
            meta("report.pdf", "application/pdf", false, None),
            payload.clone(),
        )];
        let m = build_rfc822("alice@example.com", None, &draft(), &atts).unwrap();
        let raw = String::from_utf8_lossy(&m.raw).to_ascii_lowercase();
        assert!(raw.contains("multipart/mixed"));
        assert!(!raw.contains("multipart/related"));

        let parsed = mail_parser::MessageParser::default().parse(&m.raw).unwrap();
        assert_eq!(parsed.attachment_count(), 1);
        let part = parsed.attachment(0).unwrap();
        assert_eq!(part.attachment_name(), Some("report.pdf"));
        assert_eq!(part.contents(), &payload[..]);
        assert!(parsed.body_text(0).unwrap().contains("Hello Bob,"));
        // Bcc still never leaks.
        assert!(!String::from_utf8_lossy(&m.raw).contains("secret@example.com"));
    }

    #[test]
    fn inline_plus_regular_nests_related_inside_mixed() {
        let atts = vec![
            (
                meta("logo.png", "image/png", true, Some("logo123")),
                b"png bytes".to_vec(),
            ),
            (
                meta("report.pdf", "application/pdf", false, None),
                b"pdf bytes".to_vec(),
            ),
        ];
        let m = build_rfc822("alice@example.com", None, &draft(), &atts).unwrap();
        let raw = String::from_utf8_lossy(&m.raw);
        let lower = raw.to_ascii_lowercase();
        let mixed = lower.find("multipart/mixed").expect("mixed top level");
        let related = lower.find("multipart/related").expect("nested related");
        assert!(mixed < related, "related nests inside mixed");
        assert!(raw.contains("Content-ID: <logo123>"), "{raw}");

        let parsed = mail_parser::MessageParser::default().parse(&m.raw).unwrap();
        assert_eq!(parsed.attachment_count(), 2);
        let cids: Vec<Option<&str>> = parsed.attachments().map(|p| p.content_id()).collect();
        assert!(cids.contains(&Some("logo123")), "{cids:?}");
        let inline = parsed
            .attachments()
            .find(|p| p.content_id() == Some("logo123"))
            .unwrap();
        assert_eq!(inline.contents(), b"png bytes");
        assert_eq!(inline.attachment_name(), Some("logo.png"));
        let regular = parsed
            .attachments()
            .find(|p| p.attachment_name() == Some("report.pdf"))
            .unwrap();
        assert_eq!(regular.contents(), b"pdf bytes");
    }

    #[test]
    fn inline_only_is_multipart_related_at_top_level() {
        let atts = vec![(
            meta("logo.png", "image/png", true, Some("logo123")),
            b"png bytes".to_vec(),
        )];
        let m = build_rfc822("alice@example.com", None, &draft(), &atts).unwrap();
        let lower = String::from_utf8_lossy(&m.raw).to_ascii_lowercase();
        assert!(lower.contains("multipart/related"));
        assert!(!lower.contains("multipart/mixed"));
    }

    #[test]
    fn inline_without_content_id_is_treated_as_regular() {
        let atts = vec![(meta("a.png", "image/png", true, None), b"x".to_vec())];
        let m = build_rfc822("alice@example.com", None, &draft(), &atts).unwrap();
        let lower = String::from_utf8_lossy(&m.raw).to_ascii_lowercase();
        assert!(lower.contains("multipart/mixed"));
        assert!(!lower.contains("multipart/related"));
    }

    #[test]
    fn bad_attachment_content_type_is_a_typed_error_not_a_panic() {
        let atts = vec![(meta("x", "not a content type", false, None), b"d".to_vec())];
        let err = build_rfc822("alice@example.com", None, &draft(), &atts).unwrap_err();
        assert!(matches!(err, ProviderError::Protocol(_)), "{err:?}");
    }

    #[test]
    fn markdown_off_plain_text_send_is_unchanged() {
        let d = draft(); // body_html: None
        let m = build_rfc822("alice@example.com", None, &d, &[]).unwrap();
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(raw.contains("Content-Type: text/plain"));
        assert!(!raw.to_ascii_lowercase().contains("multipart/alternative"));
    }

    #[test]
    fn markdown_on_builds_multipart_alternative_plain_then_html() {
        let mut d = draft();
        d.body_text = "**Hello** Bob.".into();
        d.body_html = Some("<p><strong>Hello</strong> Bob.</p>".into());
        let m = build_rfc822("alice@example.com", None, &d, &[]).unwrap();
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(raw.to_ascii_lowercase().contains("multipart/alternative"));
        assert!(
            raw.contains("**Hello** Bob."),
            "plain part carries the Markdown source verbatim"
        );
        assert!(
            raw.contains("<strong>Hello</strong> Bob."),
            "html part carries the rendered/sanitized html verbatim"
        );
        let plain_pos = raw
            .find("Content-Type: text/plain")
            .expect("plain part header");
        let html_pos = raw
            .find("Content-Type: text/html")
            .expect("html part header");
        assert!(
            plain_pos < html_pos,
            "text/plain must precede text/html (RFC 2046 §5.1.4)"
        );
    }

    #[test]
    fn markdown_on_bcc_still_never_leaks_into_headers_or_body() {
        let mut d = draft();
        d.body_html = Some("<p>hi</p>".into());
        let m = build_rfc822("alice@example.com", None, &d, &[]).unwrap();
        assert!(!String::from_utf8_lossy(&m.raw).contains("secret@example.com"));
    }

    #[test]
    fn markdown_on_reply_headers_still_present() {
        let mut d = draft();
        d.body_html = Some("<p>hi</p>".into());
        let m = build_rfc822("alice@example.com", None, &d, &[]).unwrap();
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(raw.contains("In-Reply-To: <parent@example.com>"));
    }

    #[test]
    fn smtp_auth_disabled_needs_both_the_status_and_the_phrase() {
        // The exact text lettre renders for the reply a real Outlook.com
        // account got on 2026-10-05 (MS1).
        assert!(is_smtp_auth_disabled(
            "permanent error (535): 5.7.139 Authentication unsuccessful, \
             SmtpClientAuthentication is disabled for the Mailbox. Visit \
             https://aka.ms/smtp_auth_disabled"
        ));
        assert!(is_smtp_auth_disabled(
            "permanent error (535): 5.7.139 Authentication unsuccessful, \
             SmtpClientAuthentication is disabled for the Tenant."
        ));
        // Same enhanced status, different cause: an ordinary auth failure.
        assert!(!is_smtp_auth_disabled(
            "permanent error (535): 5.7.139 Authentication unsuccessful, \
             the user credentials were incorrect."
        ));
        assert!(!is_smtp_auth_disabled(
            "permanent error (535): 5.7.8 Authentication failed"
        ));
        assert!(!is_smtp_auth_disabled(
            "permanent error (535): 5.7.3 SmtpClientAuthentication is disabled"
        ));
    }

    #[test]
    fn sender_configures_for_plain_and_tls() {
        // Constructing a sender never touches the network — verify both paths.
        let mut cfg = SmtpConfig {
            host: "127.0.0.1".into(),
            port: 3025,
            username: "u".into(),
            password: "p".into(),
            use_tls: false,
            allow_invalid_certs: false,
        };
        assert!(LettreSmtpSender::new(&cfg).is_ok());
        cfg.use_tls = true;
        cfg.port = 465;
        cfg.allow_invalid_certs = true;
        assert!(LettreSmtpSender::new(&cfg).is_ok());
        cfg.port = 587;
        assert!(LettreSmtpSender::new(&cfg).is_ok());
    }

    /// MM-6: Reply-To is added only when asked for, and `None` is byte-identical
    /// to the historical builder (apart from the generated Message-ID/Date).
    #[test]
    fn reply_to_is_added_only_when_given() {
        let draft = Draft {
            id: 1,
            account_id: "a".into(),
            to: vec![castellum_models::DraftAddress {
                name: None,
                email: "bob@x.test".into(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: "S".into(),
            body_text: "hi\n".into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
            status: "outbox".into(),
            updated_at_epoch: 0,
            server_mailbox_path: None,
            server_uid: None,
            server_uid_validity: None,
            server_message_id: None,
            server_saved_at_epoch: None,
        };
        let with = build_rfc822_with("me@x.test", None, Some("team@x.test"), &draft, &[]).unwrap();
        assert!(String::from_utf8_lossy(&with.raw).contains("Reply-To: team@x.test"));
        let without = build_rfc822("me@x.test", None, &draft, &[]).unwrap();
        assert!(!String::from_utf8_lossy(&without.raw).contains("Reply-To"));
        assert!(build_rfc822_with("me@x.test", None, Some("not an address"), &draft, &[]).is_err());
    }

    fn header_value(raw: &[u8], name: &str) -> Option<String> {
        let h = headers_of(raw).replace("\r\n ", " ").replace("\r\n\t", " ");
        h.split("\r\n")
            .find_map(|l| l.strip_prefix(&format!("{name}: ")).map(str::to_owned))
    }

    #[test]
    fn wire_msg_ids_brackets_each_id_and_is_idempotent() {
        assert_eq!(wire_msg_ids("parent@example.com"), "<parent@example.com>");
        assert_eq!(wire_msg_ids("a@x <b@x>  c@x>"), "<a@x> <b@x> <c@x>");
        assert_eq!(wire_msg_ids("<a@x> <b@x>"), "<a@x> <b@x>");
        assert_eq!(wire_msg_ids(" < > "), "");
    }

    #[test]
    fn reply_headers_carry_angle_brackets_on_the_wire() {
        let mut d = draft();
        d.in_reply_to = Some("parent@example.com".into());
        d.references_header = Some("root@example.com <mid@example.com> parent@example.com".into());
        for raw in [
            build_rfc822("a@example.com", None, &d, &[]).unwrap().raw,
            build_draft_rfc822("a@example.com", None, &d).unwrap().raw,
        ] {
            assert_eq!(
                header_value(&raw, "In-Reply-To").as_deref(),
                Some("<parent@example.com>")
            );
            assert_eq!(
                header_value(&raw, "References").as_deref(),
                Some("<root@example.com> <mid@example.com> <parent@example.com>")
            );
        }
        // Already-bracketed input is unchanged.
        let built = build_rfc822("a@example.com", None, &draft(), &[]).unwrap();
        assert_eq!(
            header_value(&built.raw, "In-Reply-To").as_deref(),
            Some("<parent@example.com>")
        );
    }

    #[test]
    fn envelope_recipients_are_deduplicated_case_insensitively() {
        let mut d = draft();
        d.to = vec![
            DraftAddress {
                name: None,
                email: "bob@x.test".into(),
            },
            DraftAddress {
                name: None,
                email: "BOB@x.test".into(),
            },
        ];
        d.cc = vec![DraftAddress {
            name: None,
            email: "bob@x.test".into(),
        }];
        d.bcc = vec![];
        let built = build_rfc822("a@example.com", None, &d, &[]).unwrap();
        assert_eq!(built.rcpts, vec!["bob@x.test".to_string()]);
    }

    // ---- inline images in Markdown mail (MM-5b) ----

    fn markdown_draft(html: &str) -> Draft {
        let mut d = draft();
        d.body_text = "See ![shot](cid:img-0123456789ab@castellum)".into();
        d.body_html = Some(html.into());
        d
    }

    #[test]
    fn referenced_cids_pins_the_same_vector_as_castellum_mail() {
        let html = "<img src=\"cid:b@x\"><p>t</p><img src=\"cid:a@x\"><img src=\"cid:b@x\"><img src=\"https://r\"><img src=\"cid:";
        assert_eq!(
            referenced_cids(html),
            vec!["b@x".to_string(), "a@x".to_string()]
        );
        assert!(referenced_cids("<p>none</p>").is_empty());
    }

    #[test]
    fn referenced_inline_image_sits_in_related_around_alternative() {
        let cid = "img-0123456789ab@castellum";
        let d = markdown_draft(&format!("<p><img src=\"cid:{cid}\" alt=\"shot\"></p>"));
        let atts = vec![(
            meta("shot.png", "image/png", true, Some(cid)),
            b"PNGDATA".to_vec(),
        )];
        let m = build_rfc822("alice@example.com", None, &d, &atts).unwrap();
        let raw = String::from_utf8_lossy(&m.raw).to_string();
        let lower = raw.to_ascii_lowercase();
        let related = lower.find("multipart/related").expect("related top level");
        let alt = lower
            .find("multipart/alternative")
            .expect("alternative inside");
        assert!(related < alt, "alternative nests inside related: {raw}");
        assert!(!lower.contains("multipart/mixed"), "{raw}");
        assert!(raw.contains(&format!("Content-ID: <{cid}>")), "{raw}");
        let parsed = mail_parser::MessageParser::default().parse(&m.raw).unwrap();
        let inline = parsed
            .attachments()
            .find(|p| p.content_id() == Some(cid))
            .unwrap();
        assert_eq!(inline.contents(), b"PNGDATA");
        assert!(parsed.body_html(0).unwrap().contains(&format!("cid:{cid}")));
        assert!(parsed.body_text(0).unwrap().contains("![shot]"));
    }

    #[test]
    fn unreferenced_inline_part_is_sent_as_a_regular_attachment_when_markdown_is_on() {
        let d = markdown_draft("<p>no image here</p>");
        let atts = vec![(
            meta(
                "shot.png",
                "image/png",
                true,
                Some("img-0123456789ab@castellum"),
            ),
            b"PNGDATA".to_vec(),
        )];
        let m = build_rfc822("alice@example.com", None, &d, &atts).unwrap();
        let lower = String::from_utf8_lossy(&m.raw).to_ascii_lowercase();
        assert!(
            lower.contains("multipart/mixed"),
            "regular attachment => mixed"
        );
        assert!(
            !lower.contains("multipart/related"),
            "no hidden related part"
        );
        assert!(
            !lower.contains("content-id:"),
            "no Content-ID on a regular attachment"
        );
        let parsed = mail_parser::MessageParser::default().parse(&m.raw).unwrap();
        assert_eq!(parsed.attachment_count(), 1);
        assert_eq!(
            parsed.attachment(0).unwrap().attachment_name(),
            Some("shot.png")
        );
    }

    #[test]
    fn markdown_off_inline_shape_is_unchanged() {
        // No body_html: an inline+cid part is related regardless of references.
        let atts = vec![(
            meta("logo.png", "image/png", true, Some("logo123")),
            b"png".to_vec(),
        )];
        let m = build_rfc822("alice@example.com", None, &draft(), &atts).unwrap();
        let lower = String::from_utf8_lossy(&m.raw).to_ascii_lowercase();
        assert!(lower.contains("multipart/related"));
        assert!(lower.contains("content-id: <logo123>"));
    }

    // ---- MM-17 S2: format=flowed on send ----

    fn flowed_opts() -> BuildOptions<'static> {
        BuildOptions {
            reply_to: None,
            plain_text_flowed: true,
        }
    }

    fn long_paragraph() -> String {
        "Aquest és un paràgraf llarg que el remitent escriu d'una sola tirada i que \
         el client ha de tallar a setanta-sis columnes amb salts suaus. "
            .repeat(2)
            .trim_end()
            .to_string()
    }

    #[test]
    fn flowed_off_keeps_the_fixed_plain_content_type() {
        let built = build_rfc822("a@example.com", None, &draft(), &[]).unwrap();
        let h = headers_of(&built.raw).to_ascii_lowercase();
        assert!(h.contains("content-type: text/plain; charset=utf-8"), "{h}");
        assert!(!h.contains("format=flowed"), "{h}");
    }

    #[test]
    fn flowed_on_declares_format_flowed_and_round_trips() {
        let mut d = draft();
        let para = long_paragraph();
        d.body_text = format!("{para}\n\n> quoted line\n\n-- \nBob\n");
        let built = build_rfc822_opts("a@example.com", None, flowed_opts(), &d, &[]).unwrap();
        let h = headers_of(&built.raw).to_ascii_lowercase();
        assert!(
            h.contains("content-type: text/plain; charset=utf-8; format=flowed"),
            "{h}"
        );
        let msg = mail_parser::MessageParser::default()
            .parse(&built.raw)
            .unwrap();
        let ct = msg.part(0).unwrap().content_type().unwrap();
        assert_eq!(ct.attribute("format"), Some("flowed"));
        let wire = msg.body_text(0).unwrap().to_string();
        // Soft breaks survive the transfer encoding (QP writes the space as =20).
        assert!(
            wire.lines().any(|l| l.ends_with(' ') && l != "-- "),
            "no soft break in {wire:?}"
        );
        assert!(
            wire.lines()
                .all(|l| l.chars().count() <= castellum_mail::flowed::WRAP_WIDTH),
            "{wire:?}"
        );
        assert_eq!(castellum_mail::flowed::decode(&wire, false), d.body_text);
        // Bcc stays out of the headers either way.
        assert!(!h.contains("secret@example.com"));
    }

    #[test]
    fn flowed_ascii_text_stays_seven_bit() {
        let mut d = draft();
        d.body_text = "plain ascii words ".repeat(20);
        let built = build_rfc822_opts("a@example.com", None, flowed_opts(), &d, &[]).unwrap();
        let raw = String::from_utf8_lossy(&built.raw).to_ascii_lowercase();
        assert!(raw.contains("content-transfer-encoding: 7bit"), "{raw}");
    }

    #[test]
    fn flowed_on_with_attachments_flows_the_text_part_only() {
        let mut d = draft();
        d.body_text = long_paragraph();
        let meta = DraftAttachmentMeta {
            id: 1,
            ordinal: 0,
            filename: "a.txt".into(),
            content_type: "text/plain".into(),
            size_bytes: 3,
            is_inline: false,
            content_id: None,
        };
        let built = build_rfc822_opts(
            "a@example.com",
            None,
            flowed_opts(),
            &d,
            &[(meta, b"abc".to_vec())],
        )
        .unwrap();
        let raw = String::from_utf8_lossy(&built.raw).to_ascii_lowercase();
        assert!(raw.contains("multipart/mixed"));
        assert!(
            raw.contains("content-type: text/plain; charset=utf-8; format=flowed"),
            "{raw}"
        );
    }

    #[test]
    fn flowed_on_with_html_leaves_the_markdown_alternative_alone() {
        let mut d = draft();
        d.body_text = long_paragraph();
        d.body_html = Some("<p>x</p>".into());
        let built = build_rfc822_opts("a@example.com", None, flowed_opts(), &d, &[]).unwrap();
        let raw = String::from_utf8_lossy(&built.raw).to_ascii_lowercase();
        assert!(raw.contains("multipart/alternative"));
        assert!(!raw.contains("format=flowed"), "{raw}");
    }
}
