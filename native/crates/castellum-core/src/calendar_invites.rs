// Castellum - calendar invite reply (S55).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Turns an Accept/Decline/Tentative tap into an ordinary outgoing reply — no
// second send path. Builds a minimal iTIP `METHOD:REPLY` `.ics`
// (`castellum_mail::calendar::compose_reply_ics`), attaches it to a normal
// `Draft` (`text/calendar; method=REPLY`), and queues it through the exact
// same `Database::send_draft` -> outbox -> `send_pending_with_sender` path
// every other reply uses (`castellum_provider_smtp::build_rfc822` builds the
// actual multipart bytes when it's delivered).
//
// Parsing an invite (detecting/reading a `.ics`) never records anything —
// see `castellum_mail::calendar` and `sync::persist_calendar_invite`. This
// module is the other half: recording what the USER decided, and telling
// the organizer.
//
// Never logs or exports invite contents, attendee addresses, or the `.ics`
// body — nothing here writes to a log, a diagnostic bundle, or telemetry;
// the only place any of this text goes is the queued outgoing message
// itself, over the account's own SMTP.

use crate::{CoreError, Profile};
use castellum_models::{DraftAddress, NewDraft};
use std::time::{SystemTime, UNIX_EPOCH};

fn db_io(e: castellum_db::DbError) -> CoreError {
    CoreError::Db(e.to_string())
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Stable prefix of the refusal returned when the invite's ORGANIZER address
/// matches none of the message's From / Reply-To and the caller has not
/// confirmed the recipient. The remainder of the message is the address.
/// Swift matches on it (`CalendarInviteRecipientPolicy`).
pub const RECIPIENT_MISMATCH_PREFIX: &str = "recipient mismatch: ";

/// Refusal returned when the stored ORGANIZER is not one plain addr-spec
/// (`castellum_mail::calendar::validate_plain_addr_spec`). New invites never
/// store such an address; this covers rows saved before that check existed.
/// Shown to the user as is, so it is a sentence, and it never echoes the
/// address (which may carry bidi or look-alike characters).
pub const ORGANIZER_INVALID_MSG: &str =
    "Reply not sent: this invitation's organizer address is not a plain email address.";

/// Subject text comes from the attacker-controlled SUMMARY: drop control and
/// format characters (incl. bidi overrides), flatten whitespace, cap length.
fn sanitise_subject(summary: &str) -> String {
    let cleaned: String = summary
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| {
            !c.is_control()
                && !matches!(*c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF)
        })
        .collect();
    // Collapse whitespace runs (CRLF, padding) to single spaces.
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return "(no title)".into();
    }
    cleaned.chars().take(200).collect()
}

/// The three responses Accept/Decline/Tentative may record. Any other value
/// is refused before anything is written — Swift's three action buttons are
/// the only caller and always pass one of these.
const VALID_RESPONSES: [&str; 3] = ["accepted", "declined", "tentative"];

fn response_subject_prefix(response: &str) -> &'static str {
    match response {
        "declined" => "Declined",
        "tentative" => "Tentative",
        _ => "Accepted",
    }
}

fn response_past_tense(response: &str) -> &'static str {
    match response {
        "declined" => "declined",
        "tentative" => "tentatively accepted",
        _ => "accepted",
    }
}

impl Profile {
    /// Record `response` ("accepted" | "declined" | "tentative") for
    /// `message_id`'s calendar invite and queue an iTIP REPLY to its
    /// organizer through the normal outbox. Writes `{"draft_id": i64}` to
    /// the returned JSON so Swift can show the reply in the outbox/Sent the
    /// same way as any other message.
    ///
    /// Refused (`CoreError::Sync`, nothing written) when: `response` isn't
    /// one of the three; the message doesn't exist or has no calendar
    /// invite; the invite has no organizer to reply to. All three are UI
    /// bugs, not expected runtime states — the Accept/Decline/Tentative
    /// buttons only ever exist on a message that has an invite with an
    /// organizer.
    ///
    /// The response is recorded BEFORE the send is queued: a crash between
    /// the two leaves an undelivered-but-remembered reply (visible and
    /// retryable in the outbox, same as any interrupted send) — the
    /// opposite order could deliver the reply and then forget it was sent,
    /// re-offering the buttons for a repeat send.
    ///
    /// The reply goes to the ICS ORGANIZER, which a hostile sender controls.
    /// When that address matches none of the message's From / Reply-To the
    /// call is refused with [`RECIPIENT_MISMATCH_PREFIX`] (nothing written)
    /// unless `confirmed_recipient` names that exact address, so the UI can
    /// name the real recipient and ask first (#3380). Evaluation-mode
    /// accounts are refused up front, before any draft is saved (#3366), and
    /// a failed queue removes the draft and restores the previous response.
    ///
    /// The confirmation is BOUND to what the user was shown (#3472):
    /// `confirmed_recipient` is the address the confirmation named, not a
    /// flag. Whenever it is given and is not byte-for-byte the address this
    /// call would send to (the invite was re-synced with another ORGANIZER,
    /// the card showed a stale one, a caller bug), the call is refused with
    /// the same mismatch error naming the CURRENT address, so the UI asks
    /// again about the right one. An ORGANIZER that is not one plain
    /// addr-spec is refused with [`ORGANIZER_INVALID_MSG`] whatever was
    /// confirmed.
    pub fn respond_to_calendar_invite_json(
        &self,
        message_id: &str,
        response: &str,
        confirmed_recipient: Option<&str>,
    ) -> Result<String, CoreError> {
        if !VALID_RESPONSES.contains(&response) {
            return Err(CoreError::Sync(format!(
                "invalid calendar invite response '{response}'"
            )));
        }
        let Some(detail) = self.db.get_message_detail(message_id).map_err(db_io)? else {
            return Err(CoreError::Sync(format!("unknown message {message_id}")));
        };
        let Some(mut invite) = detail.calendar_invite else {
            return Err(CoreError::Sync(format!(
                "message {message_id} has no calendar invite"
            )));
        };
        let Some(stored_organizer) = invite.organizer_email.clone() else {
            return Err(CoreError::Sync(
                "invite has no organizer to reply to".into(),
            ));
        };
        // Re-validate at the point of use: rows stored before the parser
        // refused hostile ORGANIZER values are still in the database. Fail
        // closed, never send to an address that cannot be shown truthfully.
        let Some(organizer_email) =
            castellum_mail::calendar::validate_plain_addr_spec(stored_organizer.trim())
        else {
            return Err(CoreError::Sync(ORGANIZER_INVALID_MSG.into()));
        };
        // Everything below (the reply ICS, the draft's To) uses the validated
        // address and a sanitised display name, never the stored text.
        invite.organizer_email = Some(organizer_email.clone());
        invite.organizer_name = invite
            .organizer_name
            .as_deref()
            .and_then(castellum_mail::calendar::sanitise_display_name);

        let account = self
            .db
            .list_accounts()
            .map_err(db_io)?
            .into_iter()
            .find(|a| a.id == detail.header.account_id)
            .ok_or_else(|| {
                CoreError::Sync(format!("unknown account {}", detail.header.account_id))
            })?;

        // Preflight: a refused send must not leave a draft or a false
        // "You accepted" behind (#3366). Same stable text Swift matches.
        if !self.db.account_write_enabled(&account.id).map_err(db_io)? {
            return Err(CoreError::Db(castellum_db::EVALUATION_MODE_MSG.into()));
        }

        let mismatch = || CoreError::Sync(format!("{RECIPIENT_MISMATCH_PREFIX}{organizer_email}"));
        if let Some(confirmed) = confirmed_recipient {
            // Bound confirmation: the user agreed to ONE address. Exact
            // comparison on purpose; the UI echoes back what the mismatch
            // error named, so anything else means it showed something else.
            if confirmed != organizer_email {
                return Err(mismatch());
            }
        } else {
            let org = organizer_email.to_lowercase();
            let known = detail
                .header
                .from_address
                .iter()
                .map(|a| a.trim().to_lowercase())
                .chain(
                    detail
                        .addresses
                        .iter()
                        .filter(|a| a.kind == "from" || a.kind == "reply-to")
                        .map(|a| a.address.trim().to_lowercase()),
                )
                .any(|a| a == org);
            if !known {
                return Err(mismatch());
            }
        }

        // Answer as the address the invitation was actually sent to: the first
        // ATTENDEE that is the account address or one of its identities/aliases
        // (case-insensitive), else the account address. The draft's From follows,
        // so the From header and ATTENDEE agree (audit #3446).
        let identities = self.db.list_identities(&account.id).map_err(db_io)?;
        let own = |email: &str| {
            email.eq_ignore_ascii_case(&account.email)
                || identities
                    .iter()
                    .any(|i| i.email.eq_ignore_ascii_case(email))
        };
        let (reply_email, reply_name, reply_from) =
            match invite.attendees.iter().find(|a| own(a.email.trim())) {
                Some(a) if !a.email.trim().eq_ignore_ascii_case(&account.email) => {
                    let ident = identities
                        .iter()
                        .find(|i| i.email.eq_ignore_ascii_case(a.email.trim()));
                    let name = ident
                        .and_then(|i| i.display_name.clone())
                        .or_else(|| account.display_name.clone());
                    (a.email.trim().to_owned(), name, true)
                }
                _ => (account.email.clone(), account.display_name.clone(), false),
            };

        let ics = castellum_mail::calendar::compose_reply_ics(
            &invite,
            response,
            &reply_email,
            reply_name.as_deref(),
        )
        .map_err(|e| CoreError::Sync(format!("compose reply ics: {e}")))?;

        // The identity the reply is sent AS (an alias when the invite named
        // one), so the body never reveals the primary address behind it.
        let our_name = reply_name.clone().unwrap_or_else(|| reply_email.clone());
        let subject = format!(
            "{}: {}",
            response_subject_prefix(response),
            sanitise_subject(invite.summary.as_deref().unwrap_or("(no title)"))
        );
        let draft = NewDraft {
            id: None,
            account_id: account.id.clone(),
            to: vec![DraftAddress {
                name: invite.organizer_name.clone(),
                email: organizer_email,
            }],
            cc: vec![],
            bcc: vec![],
            subject,
            body_text: format!(
                "{our_name} has {} this invitation.\n",
                response_past_tense(response)
            ),
            body_html: None,
            from_email: reply_from.then(|| reply_email.clone()),
            from_name: if reply_from { reply_name.clone() } else { None },
            in_reply_to: None,
            references_header: None,
        };
        let draft_id = self.db.save_draft(&draft).map_err(db_io)?;
        self.db
            .add_draft_attachment(
                draft_id,
                "invite.ics",
                "text/calendar; method=REPLY; charset=UTF-8",
                false,
                None,
                ics.as_bytes(),
            )
            .map_err(db_io)?;

        let (prev_response, prev_at) = (invite.our_response.clone(), invite.responded_at);
        self.db
            .record_calendar_invite_response(message_id, response, now_epoch())
            .map_err(db_io)?;
        if let Err(e) = self.db.send_draft(draft_id) {
            // Undo the optimistic writes so the card does not claim a reply
            // that was never queued (#3366).
            let _ = self.db.delete_draft(draft_id);
            let _ = self.db.restore_calendar_invite_response(
                message_id,
                prev_response.as_deref(),
                prev_at,
            );
            return Err(db_io(e));
        }

        serde_json::to_string(&serde_json::json!({ "draft_id": draft_id }))
            .map_err(|_| CoreError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Mailbox, NewMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_profile() -> Profile {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = castellum_testutil::scratch_root().join(format!(
            "castellum-calendar-invite-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::open_profile(&dir).expect("open profile")
    }

    const WELL_FORMED: &str = "BEGIN:VCALENDAR\r\n\
        VERSION:2.0\r\n\
        METHOD:REQUEST\r\n\
        BEGIN:VEVENT\r\n\
        UID:event-abc@example.com\r\n\
        SEQUENCE:0\r\n\
        DTSTART:20260925T140000Z\r\n\
        DTEND:20260925T150000Z\r\n\
        SUMMARY:Roadmap review\r\n\
        ORGANIZER;CN=Alice Organizer:mailto:alice@example.com\r\n\
        ATTENDEE;CN=Bob Attendee:mailto:bob@example.com\r\n\
        END:VEVENT\r\n\
        END:VCALENDAR\r\n";

    fn seed_message_with_invite(profile: &Profile) -> String {
        seed_message_with_invite_from(profile, "alice@example.com")
    }

    fn seed_message_with_invite_from(profile: &Profile, from: &str) -> String {
        profile
            .db
            .upsert_account(&Account {
                id: "acc".into(),
                email: "bob@example.com".into(),
                display_name: Some("Bob Attendee".into()),
                provider: "imap".into(),
            })
            .unwrap();
        // The reply is a real queued send (`send_draft`), which — like every
        // write — is refused in evaluation mode; leave it for these tests.
        profile.db.set_account_write_enabled("acc", true).unwrap();
        profile
            .db
            .upsert_mailbox(&Mailbox {
                id: "acc:INBOX".into(),
                account_id: "acc".into(),
                path: "INBOX".into(),
                name: "INBOX".into(),
                role: Some("inbox".into()),
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        let message_id = "acc:INBOX:1".to_string();
        profile
            .db
            .upsert_message(&NewMessage {
                id: message_id.clone(),
                account_id: "acc".into(),
                mailbox_id: "acc:INBOX".into(),
                provider_uid: Some(1),
                rfc822_message_id: Some("<invite@example.com>".into()),
                subject: Some("Roadmap review".into()),
                from_name: Some("Alice Organizer".into()),
                from_address: Some(from.into()),
                sent_at: Some(1_700_000_000),
                snippet: None,
                size_bytes: Some(100),
                has_attachments: true,
                attachments: Some(vec![]),
                is_read: false,
                is_starred: false,
                addresses: vec![],
                text_body: None,
                sanitized_html: None,
                html_body: None,
                has_remote_content: false,
                references: vec![],
            })
            .unwrap();
        let invite = castellum_mail::calendar::parse_calendar_invite_checked(WELL_FORMED).unwrap();
        profile
            .db
            .upsert_calendar_invite(&message_id, &invite)
            .unwrap();
        message_id
    }

    #[test]
    fn accept_records_response_and_queues_a_reply_through_the_normal_outbox() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);

        let json = profile
            .respond_to_calendar_invite_json(&message_id, "accepted", None)
            .expect("accept succeeds");
        assert!(json.contains("draft_id"));

        let invite = profile
            .db
            .get_calendar_invite(&message_id)
            .unwrap()
            .expect("invite still present");
        assert_eq!(invite.our_response.as_deref(), Some("accepted"));
        assert!(invite.responded_at.is_some());

        // The draft is now in the outbox (queued via `send_draft`), not
        // sitting as an editable `draft` — same shape as any other reply.
        let drafts = profile.db.list_drafts("acc").unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].status, "outbox");
        assert_eq!(drafts[0].to[0].email, "alice@example.com");
        assert!(drafts[0].subject.starts_with("Accepted:"));

        let attachments = profile.db.list_draft_attachments(drafts[0].id).unwrap();
        assert_eq!(attachments.len(), 1);
        assert!(attachments[0].content_type.starts_with("text/calendar"));
    }

    #[test]
    fn an_invite_sent_to_an_alias_is_answered_as_the_alias() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);
        // The invite names bob@example.com (the account). Re-seed it addressed to an alias.
        profile
            .db
            .create_identity(&castellum_models::NewIdentity {
                account_id: "acc".into(),
                email: "Bob.Alias@example.com".into(),
                display_name: Some("Bob Alias".into()),
                reply_to: None,
                bcc: None,
                is_default: false,
            })
            .unwrap();
        let aliased = WELL_FORMED.replace("mailto:bob@example.com", "mailto:bob.alias@example.com");
        let invite = castellum_mail::calendar::parse_calendar_invite_checked(&aliased).unwrap();
        profile
            .db
            .upsert_calendar_invite(&message_id, &invite)
            .unwrap();

        profile
            .respond_to_calendar_invite_json(&message_id, "accepted", None)
            .expect("accept succeeds");
        let drafts = profile.db.list_drafts("acc").unwrap();
        assert_eq!(
            drafts[0].from_email.as_deref(),
            Some("bob.alias@example.com")
        );
        // The human-readable body names the alias, not the primary account.
        let text = &drafts[0].body_text;
        assert!(text.starts_with("Bob Alias has accepted"), "{text}");
        let atts = profile.db.list_draft_attachments(drafts[0].id).unwrap();
        let bytes = profile
            .db
            .draft_attachment_bytes(atts[0].id)
            .unwrap()
            .unwrap();
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert!(body.to_lowercase().contains("attendee"), "{body}");
        assert!(
            body.contains("bob.alias@example.com"),
            "answers as the invited alias: {body}"
        );
        assert!(!body.contains("mailto:bob@example.com"), "{body}");
    }

    #[test]
    fn declining_twice_is_refused_the_second_time_only_by_the_ui_not_here() {
        // This module itself doesn't refuse a second response (idempotent
        // re-recording is fine — SEQUENCE/organizer haven't changed); it's
        // simply an ordinary second reply. Documented here so a future
        // change to that behaviour is deliberate, not accidental.
        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);
        profile
            .respond_to_calendar_invite_json(&message_id, "declined", None)
            .unwrap();
        profile
            .respond_to_calendar_invite_json(&message_id, "accepted", None)
            .unwrap();
        let invite = profile
            .db
            .get_calendar_invite(&message_id)
            .unwrap()
            .unwrap();
        assert_eq!(invite.our_response.as_deref(), Some("accepted"));
        assert_eq!(profile.db.list_drafts("acc").unwrap().len(), 2);
    }

    /// #3041: LOCATION and DESCRIPTION must survive the database round trip,
    /// not just parsing.
    #[test]
    fn location_and_description_survive_a_reload() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);
        let ics = WELL_FORMED.replace(
            "SUMMARY:Roadmap review\r\n",
            "SUMMARY:Roadmap review\r\nLOCATION:Room 4\r\nDESCRIPTION:Bring the plan\r\n",
        );
        let invite = castellum_mail::calendar::parse_calendar_invite_checked(&ics).unwrap();
        profile
            .db
            .upsert_calendar_invite(&message_id, &invite)
            .unwrap();
        let loaded = profile
            .db
            .get_calendar_invite(&message_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.location.as_deref(), Some("Room 4"));
        assert_eq!(loaded.description.as_deref(), Some("Bring the plan"));
    }

    #[test]
    fn invalid_response_is_refused_before_anything_is_written() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);
        let err = profile
            .respond_to_calendar_invite_json(&message_id, "maybe-later", None)
            .expect_err("invalid response is refused");
        assert!(format!("{err:?}").contains("invalid calendar invite response"));
        assert!(profile.db.list_drafts("acc").unwrap().is_empty());
        let invite = profile
            .db
            .get_calendar_invite(&message_id)
            .unwrap()
            .unwrap();
        assert!(invite.our_response.is_none());
    }

    fn assert_nothing_written(profile: &Profile, message_id: &str) {
        assert!(
            profile.db.list_drafts("acc").unwrap().is_empty(),
            "no orphan draft"
        );
        let invite = profile.db.get_calendar_invite(message_id).unwrap().unwrap();
        assert!(invite.our_response.is_none(), "no false response");
        assert!(invite.responded_at.is_none());
    }

    #[test]
    fn evaluation_mode_refusal_writes_nothing_and_is_repeatable() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);
        profile.db.set_account_write_enabled("acc", false).unwrap();
        for _ in 0..2 {
            let err = profile
                .respond_to_calendar_invite_json(&message_id, "accepted", None)
                .expect_err("refused in evaluation mode");
            assert!(err
                .to_string()
                .starts_with(castellum_db::EVALUATION_MODE_MSG));
            assert_nothing_written(&profile, &message_id);
        }
    }

    #[test]
    fn organizer_not_matching_from_needs_confirmation() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite_from(&profile, "assistant@elsewhere.example");
        let err = profile
            .respond_to_calendar_invite_json(&message_id, "accepted", None)
            .expect_err("mismatch refused without confirmation");
        assert_eq!(
            err.to_string(),
            format!("{RECIPIENT_MISMATCH_PREFIX}alice@example.com")
        );
        assert_nothing_written(&profile, &message_id);

        profile
            .respond_to_calendar_invite_json(&message_id, "accepted", Some("alice@example.com"))
            .expect("confirmed send goes through");
        let drafts = profile.db.list_drafts("acc").unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].to[0].email, "alice@example.com");
    }

    /// Replace the seeded invite's organizer, as a re-sync with a different
    /// `.ics` (or a row stored by an older build) would.
    fn set_organizer(profile: &Profile, message_id: &str, email: &str, name: Option<&str>) {
        let mut invite = profile.db.get_calendar_invite(message_id).unwrap().unwrap();
        invite.organizer_email = Some(email.into());
        invite.organizer_name = name.map(str::to_owned);
        profile
            .db
            .upsert_calendar_invite(message_id, &invite)
            .unwrap();
    }

    /// #3472 item 4: the confirmation is bound to the address the user saw.
    /// Confirming one address must never send to another.
    #[test]
    fn confirmation_for_a_different_address_is_refused() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite_from(&profile, "assistant@elsewhere.example");
        // The user was shown (and confirmed) alice@example.com; meanwhile the
        // invite now names someone else.
        set_organizer(&profile, &message_id, "eve@evil.example", None);
        for confirmed in [
            "alice@example.com",
            "",
            "1",
            "true",
            "EVE@evil.example",
            " eve@evil.example",
            "eve@evil.example ",
            "eve@evil.example\u{200B}",
        ] {
            let err = profile
                .respond_to_calendar_invite_json(&message_id, "accepted", Some(confirmed))
                .expect_err("a confirmation for another address is refused");
            assert_eq!(
                err.to_string(),
                format!("{RECIPIENT_MISMATCH_PREFIX}eve@evil.example"),
                "{confirmed:?}: the refusal names the CURRENT recipient"
            );
            assert_nothing_written(&profile, &message_id);
        }
        // Confirming the address actually shown goes through, to that address.
        profile
            .respond_to_calendar_invite_json(&message_id, "accepted", Some("eve@evil.example"))
            .expect("bound confirmation sends");
        let drafts = profile.db.list_drafts("acc").unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].to[0].email, "eve@evil.example");
    }

    /// A wrong confirmation is refused even when the organizer is the sender
    /// (no confirmation would have been needed): it still proves the UI and
    /// the core disagree about who the reply goes to.
    #[test]
    fn wrong_confirmation_is_refused_even_when_organizer_is_the_sender() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);
        let err = profile
            .respond_to_calendar_invite_json(&message_id, "accepted", Some("eve@evil.example"))
            .expect_err("refused");
        assert_eq!(
            err.to_string(),
            format!("{RECIPIENT_MISMATCH_PREFIX}alice@example.com")
        );
        assert_nothing_written(&profile, &message_id);
    }

    /// #3472 item 4: an ORGANIZER stored before the parser validated it (bidi,
    /// homoglyph, several addresses) is refused with a readable reason, with
    /// or without a confirmation, and nothing is written.
    #[test]
    fn stored_hostile_organizer_is_refused_with_a_readable_reason() {
        for hostile in [
            "\u{202E}moc.elpmaxe@ecila\u{202C}@evil.example",
            "alice@example.com\u{202E}moc.live@",
            "\u{430}lice@example.com",
            "alice@\u{435}xample.com",
            "alice@example.com,eve@evil.example",
            "alice@example.com?bcc=eve@evil.example",
            "Alice <alice@example.com>",
            "alice@localhost",
            "",
        ] {
            let profile = temp_profile();
            // From equals the hostile text, so only validation can stop it.
            let message_id = seed_message_with_invite_from(&profile, hostile);
            set_organizer(&profile, &message_id, hostile, None);
            for confirmed in [None, Some(hostile)] {
                let err = profile
                    .respond_to_calendar_invite_json(&message_id, "accepted", confirmed)
                    .expect_err("hostile organizer refused");
                assert_eq!(err.to_string(), ORGANIZER_INVALID_MSG, "{hostile:?}");
                assert_nothing_written(&profile, &message_id);
            }
        }
    }

    /// A stored CN (older row) is sanitised before it becomes the draft's To
    /// name or the reply ICS's ORGANIZER CN.
    #[test]
    fn stored_organizer_name_is_sanitised_in_the_reply() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);
        let long = format!("Your\u{202E} Manager {}", "x".repeat(500));
        set_organizer(&profile, &message_id, "alice@example.com", Some(&long));
        profile
            .respond_to_calendar_invite_json(&message_id, "accepted", None)
            .unwrap();
        let drafts = profile.db.list_drafts("acc").unwrap();
        let name = drafts[0].to[0].name.clone().expect("a name");
        assert!(name.starts_with("Your Manager x"), "{name}");
        assert!(name.chars().count() <= castellum_mail::calendar::MAX_DISPLAY_NAME_CHARS);

        let profile = temp_profile();
        let message_id = seed_message_with_invite(&profile);
        set_organizer(
            &profile,
            &message_id,
            "alice@example.com",
            Some("ceo@bank.example"),
        );
        profile
            .respond_to_calendar_invite_json(&message_id, "accepted", None)
            .unwrap();
        assert_eq!(profile.db.list_drafts("acc").unwrap()[0].to[0].name, None);
    }

    #[test]
    fn from_match_is_case_insensitive_and_needs_no_flag() {
        let profile = temp_profile();
        let message_id = seed_message_with_invite_from(&profile, "Alice@Example.COM");
        profile
            .respond_to_calendar_invite_json(&message_id, "accepted", None)
            .expect("matching From sends without the flag");
    }

    #[test]
    fn subject_drops_control_and_bidi_characters_and_is_capped() {
        assert_eq!(
            sanitise_subject("Hi\r\nBcc: x\u{202E}evil"),
            "Hi Bcc: xevil"
        );
        assert_eq!(sanitise_subject("\u{200B}\n"), "(no title)");
        assert_eq!(sanitise_subject(&"a".repeat(500)).chars().count(), 200);
    }

    #[test]
    fn message_with_no_invite_is_refused() {
        let profile = temp_profile();
        profile
            .db
            .upsert_account(&Account {
                id: "acc".into(),
                email: "bob@example.com".into(),
                display_name: None,
                provider: "imap".into(),
            })
            .unwrap();
        profile
            .db
            .upsert_mailbox(&Mailbox {
                id: "acc:INBOX".into(),
                account_id: "acc".into(),
                path: "INBOX".into(),
                name: "INBOX".into(),
                role: Some("inbox".into()),
                delimiter: Some("/".into()),
                subscribed: true,
                selectable: true,
                sync_by_default: true,
            })
            .unwrap();
        profile
            .db
            .upsert_message(&NewMessage {
                id: "acc:INBOX:1".into(),
                account_id: "acc".into(),
                mailbox_id: "acc:INBOX".into(),
                provider_uid: Some(1),
                rfc822_message_id: None,
                subject: Some("Just an email".into()),
                from_name: None,
                from_address: Some("nobody@example.com".into()),
                sent_at: None,
                snippet: None,
                size_bytes: None,
                has_attachments: false,
                attachments: None,
                is_read: false,
                is_starred: false,
                addresses: vec![],
                text_body: None,
                sanitized_html: None,
                html_body: None,
                has_remote_content: false,
                references: vec![],
            })
            .unwrap();
        let err = profile
            .respond_to_calendar_invite_json("acc:INBOX:1", "accepted", None)
            .expect_err("no invite to respond to");
        assert!(format!("{err:?}").contains("no calendar invite"));
    }
}
