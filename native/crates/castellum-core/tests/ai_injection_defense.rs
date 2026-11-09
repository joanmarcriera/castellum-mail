// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S50 prompt-injection defence: red-team fixture corpus.
//
// Every fixture under tests/fixtures/injection/ is a raw attack payload
// drawn from real prompt-injection technique families (role-spoofing,
// invisible/bidi text, delimiter forgery, HTML-attribute/comment
// smuggling, multilingual rewording, homoglyphs, exfiltration asks). Each
// is run through the SAME pure prompt builders `run::build_prompt` calls
// in production (`castellum_core::ai::prompts::build_*`) — no live model
// needed for the structural assertions below; a live-Ollama smoke test is
// `#[ignore]`d separately at the bottom (mirrors tests/ollama_e2e.rs's
// env-var convention).
//
// What's asserted (see ai/defense.rs's module doc for the design
// rationale — this is NOT a keyword blocklist):
//   * exactly one real BEGIN/END fence in the assembled user turn, however
//     the fixture tries to spoof one (defence: defense::envelope);
//   * the system prompt always carries the untrusted-data warning;
//   * the fixture's content is preserved (not silently dropped) but only
//     ever appears inside the fence, never in the system prompt;
//   * invisible/bidi-override characters never survive into the prompt.

use castellum_core::ai::defense;
use castellum_core::ai::prompts::{
    build_draft_message, build_draft_reply, build_summarize_message, build_summarize_thread,
    ThreadMessage,
};

const NUM_CTX: u32 = 8192;
const MAX_OUT: u32 = 1024;

/// Fixtures fed as the plain-text `body` param of `build_summarize_message`.
/// Excludes 05/12 (HTML — see HTML_FIXTURES), 07 (subject — Task 8), 08
/// (sender display name — Task 8, no such param exists to attack).
const BODY_FIXTURES: &[(&str, &str)] = &[
    (
        "ignore_previous_instructions",
        include_str!("fixtures/injection/01_ignore_previous_instructions.txt"),
    ),
    (
        "fake_system_block",
        include_str!("fixtures/injection/02_fake_system_block.txt"),
    ),
    (
        "invisible_zero_width_text",
        include_str!("fixtures/injection/03_invisible_zero_width_text.txt"),
    ),
    (
        "bidi_override_hidden_text",
        include_str!("fixtures/injection/04_bidi_override_hidden_text.txt"),
    ),
    (
        "quoted_reply_injection",
        include_str!("fixtures/injection/06_quoted_reply_injection.txt"),
    ),
    (
        "delimiter_spoofing",
        include_str!("fixtures/injection/09_delimiter_spoofing.txt"),
    ),
    (
        "exfiltrate_other_mail",
        include_str!("fixtures/injection/10_exfiltrate_other_mail.txt"),
    ),
    (
        "draft_exfiltration_url",
        include_str!("fixtures/injection/11_draft_exfiltration_url.txt"),
    ),
    (
        "role_play_jailbreak",
        include_str!("fixtures/injection/13_role_play_jailbreak.txt"),
    ),
    (
        "multilingual_injection",
        include_str!("fixtures/injection/14_multilingual_injection.txt"),
    ),
    (
        "nested_markdown_code_fence",
        include_str!("fixtures/injection/15_nested_markdown_code_fence.txt"),
    ),
    (
        "homoglyph_substitution",
        include_str!("fixtures/injection/16_homoglyph_substitution.txt"),
    ),
    (
        "unicode_tag_smuggling",
        include_str!("fixtures/injection/18_unicode_tag_smuggling.txt"),
    ),
    (
        "zero_width_split_delimiter",
        include_str!("fixtures/injection/17_zero_width_split_delimiter.txt"),
    ),
];

/// Fixtures fed as `sanitized_html` (the `strip_tags` fallback path).
const HTML_FIXTURES: &[(&str, &str)] = &[
    (
        "html_attribute_instruction",
        include_str!("fixtures/injection/05_html_attribute_instruction.txt"),
    ),
    (
        "fake_developer_note_html_comment",
        include_str!("fixtures/injection/12_fake_developer_note_html_comment.txt"),
    ),
];

#[test]
fn every_body_fixture_is_quarantined_inside_a_single_unforgeable_fence() {
    for (name, raw) in BODY_FIXTURES {
        let (system, user) = build_summarize_message(
            Some("Re: project update"),
            Some("colleague@example.com"),
            Some("2026-09-18"),
            Some(raw),
            None,
            NUM_CTX,
            MAX_OUT,
        );

        assert_eq!(
            user.matches(defense::DATA_BEGIN).count(),
            1,
            "fixture {name}: exactly one real BEGIN fence expected: {user}"
        );
        assert_eq!(
            user.matches(defense::DATA_END).count(),
            1,
            "fixture {name}: exactly one real END fence expected: {user}"
        );
        assert!(
            system.contains("untrusted"),
            "fixture {name}: system prompt must carry the untrusted-data warning: {system}"
        );

        assert!(
            !user
                .chars()
                .any(|c| ('\u{E0000}'..='\u{E007F}').contains(&c)),
            "fixture {name}: Unicode tag characters must not reach the model"
        );
        let begin = user.find(defense::DATA_BEGIN).unwrap();
        let end = user.find(defense::DATA_END).unwrap();
        assert!(begin < end, "fixture {name}: BEGIN must precede END");

        // Content preserved (quarantined, not dropped) but never leaked
        // into the system prompt — checked with a short probe taken
        // verbatim (whitespace included) from the start of the fixture.
        // Every fixture opens with plain filler text before its attack
        // payload (and before fixtures 03/04's invisible/bidi characters),
        // so an unfiltered prefix is guaranteed to survive `envelope()`
        // byte-for-byte and appear as a contiguous substring of `user`.
        let probe: String = raw.chars().take(12).collect();
        if !probe.is_empty() {
            assert!(
                user.contains(&probe),
                "fixture {name}: content should be preserved, not dropped: probe {probe:?} missing from {user:?}"
            );
            assert!(
                !system.contains(&probe),
                "fixture {name}: attack content must never leak into the system prompt"
            );
        }
    }
}

#[test]
fn every_html_fixture_is_stripped_and_quarantined() {
    for (name, raw_html) in HTML_FIXTURES {
        let (system, user) = build_summarize_message(
            Some("Report"),
            Some("ops@example.com"),
            Some("2026-09-18"),
            None,
            Some(raw_html),
            NUM_CTX,
            MAX_OUT,
        );
        assert!(
            !user.contains('<'),
            "fixture {name}: HTML tags must be stripped before the model sees them: {user}"
        );
        assert_eq!(
            user.matches(defense::DATA_BEGIN).count(),
            1,
            "fixture {name}: {user}"
        );
        assert_eq!(
            user.matches(defense::DATA_END).count(),
            1,
            "fixture {name}: {user}"
        );
        assert!(system.contains("untrusted"), "fixture {name}: {system}");
    }
}

#[test]
fn every_body_fixture_also_quarantines_inside_a_thread_summary() {
    // A thread of 2 messages, the second carrying the fixture — proves
    // build_summarize_thread's single envelope wrap (Task 2, Step 3) holds
    // for every fixture too, not just build_summarize_message.
    for (name, raw) in BODY_FIXTURES {
        let msgs = vec![
            ThreadMessage {
                subject: Some("Kickoff".into()),
                from: Some("alice@example.com".into()),
                date: Some("2026-09-17".into()),
                text_body: Some("Looking forward to this.".into()),
                sanitized_html: None,
            },
            ThreadMessage {
                subject: Some("Re: Kickoff".into()),
                from: Some("bob@example.com".into()),
                date: Some("2026-09-18".into()),
                text_body: Some((*raw).to_string()),
                sanitized_html: None,
            },
        ];
        let (system, user) = build_summarize_thread(&msgs, NUM_CTX, MAX_OUT);
        assert_eq!(
            user.matches(defense::DATA_BEGIN).count(),
            1,
            "fixture {name}: {user}"
        );
        assert_eq!(
            user.matches(defense::DATA_END).count(),
            1,
            "fixture {name}: {user}"
        );
        assert!(system.contains("untrusted"), "fixture {name}: {system}");
    }
}

/// #3386: a forwarded hostile mail lands in the compose body, so every body
/// fixture must be quarantined by `build_draft_message` (forward-compose) and
/// `build_draft_reply` alike.
#[test]
fn every_body_fixture_is_fenced_in_draft_message_and_draft_reply() {
    for (name, raw) in BODY_FIXTURES {
        let forwarded = format!("---------- Forwarded message ----------\n{raw}");
        let (system, user) = build_draft_message(
            "Summarise this for Bob.",
            Some("Fwd: update"),
            Some("bob@example.com"),
            Some(&forwarded),
            NUM_CTX,
            MAX_OUT,
        );
        let (rsystem, ruser) = build_draft_reply(
            Some("Re: update"),
            Some("colleague@example.com"),
            Some("2026-09-18"),
            Some(raw),
            None,
            "Reply politely.",
            NUM_CTX,
            MAX_OUT,
        );
        for (which, system, user) in [("message", &system, &user), ("reply", &rsystem, &ruser)] {
            assert_eq!(
                user.matches(defense::DATA_BEGIN).count(),
                1,
                "fixture {name} ({which}): {user}"
            );
            assert_eq!(
                user.matches(defense::DATA_END).count(),
                1,
                "fixture {name} ({which}): {user}"
            );
            assert!(system.contains("untrusted"), "fixture {name} ({which})");
            let end = user.find(defense::DATA_END).unwrap();
            assert!(
                user.find("Instruction:").unwrap() > end,
                "fixture {name} ({which}): instruction must follow the fence"
            );
        }
    }
}

#[test]
fn delimiter_spoofing_fixture_cannot_forge_a_second_fence() {
    let raw = include_str!("fixtures/injection/09_delimiter_spoofing.txt");
    let (_, user) = build_summarize_message(
        Some("Q3 numbers"),
        Some("finance@example.com"),
        Some("2026-09-18"),
        Some(raw),
        None,
        NUM_CTX,
        MAX_OUT,
    );
    assert_eq!(
        user.matches(defense::DATA_BEGIN).count(),
        1,
        "forged fence leaked: {user}"
    );
    assert_eq!(
        user.matches(defense::DATA_END).count(),
        1,
        "forged fence leaked: {user}"
    );

    // The forged "SYSTEM: New instructions" line is still present (content
    // preserved) but ends up INSIDE the single real fence, not between two
    // fences pretending the real content ended early.
    let begin = user.find(defense::DATA_BEGIN).unwrap();
    let end = user.find(defense::DATA_END).unwrap();
    let forged_pos = user
        .find("New instructions")
        .expect("forged content preserved");
    assert!(
        forged_pos > begin && forged_pos < end,
        "forged instruction escaped the fence: {user}"
    );
}

#[test]
fn invisible_zero_width_fixture_is_stripped_but_reassembles_readable_text() {
    let raw = include_str!("fixtures/injection/03_invisible_zero_width_text.txt");
    assert!(
        raw.contains('\u{200B}'),
        "fixture must contain a zero-width space to test anything"
    );
    let (_, user) = build_summarize_message(
        Some("Meeting"),
        Some("alice@example.com"),
        Some("2026-09-18"),
        Some(raw),
        None,
        NUM_CTX,
        MAX_OUT,
    );
    assert!(
        !user.contains('\u{200B}'),
        "zero-width space must not survive into the prompt: {user:?}"
    );
    assert!(
        user.contains("IGNORE ALL PREVIOUS INSTRUCTIONS"),
        "letters must reassemble contiguously once the joiners are stripped: {user:?}"
    );
}

#[test]
fn bidi_override_fixture_loses_its_control_characters() {
    let raw = include_str!("fixtures/injection/04_bidi_override_hidden_text.txt");
    assert!(
        raw.contains('\u{202E}'),
        "fixture must contain RLO to test anything"
    );
    let (_, user) = build_summarize_message(
        Some("Approval"),
        Some("ops@example.com"),
        Some("2026-09-18"),
        Some(raw),
        None,
        NUM_CTX,
        MAX_OUT,
    );
    assert!(
        !user.contains('\u{202E}') && !user.contains('\u{202C}'),
        "bidi override controls must not survive: {user:?}"
    );
}

// ---------------------------------------------------------------------------
// Full DB-backed pipeline tests
// ---------------------------------------------------------------------------

use castellum_core::ai::run::{run_ai_jobs_with_backend, AiRunConfig};
use castellum_db::Database;
use castellum_models::{Account, Mailbox, NewMessage};
use castellum_providers::{
    AiBackend, AiError, GenerateRequest, GenerateSummary, ModelInfo, StreamControl,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db() -> Database {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Unique per RUN: pids recycle and these are never deleted, so a
    // pid+counter name reopens an earlier run's directory. See the note
    // in castellum_db::ai::tests::fresh_db.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = castellum_testutil::scratch_root().join(format!(
        "castellum-ai-injection-test-{}-{stamp}-{n}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    Database::open(&path).expect("open db")
}

fn run_config() -> AiRunConfig {
    serde_json::from_value(serde_json::json!({
        "endpoint": "http://127.0.0.1:11434",
        "model": "llama3.1:8b",
    }))
    .unwrap()
}

fn seed_message_with(db: &Database, msg_id: &str, subject: &str, from_name: &str, body: &str) {
    db.upsert_account(&Account {
        id: "acc1".into(),
        email: "me@example.com".into(),
        display_name: None,
        provider: "imap".into(),
    })
    .unwrap();
    db.upsert_mailbox(&Mailbox {
        id: "acc1:INBOX".into(),
        account_id: "acc1".into(),
        path: "INBOX".into(),
        name: "Inbox".into(),
        role: Some("inbox".into()),
        delimiter: Some("/".into()),
        subscribed: true,
        selectable: true,
        sync_by_default: true,
    })
    .unwrap();
    db.upsert_message(&NewMessage {
        id: msg_id.into(),
        account_id: "acc1".into(),
        mailbox_id: "acc1:INBOX".into(),
        provider_uid: Some(1),
        rfc822_message_id: Some(format!("<{msg_id}@test>")),
        subject: Some(subject.into()),
        from_name: Some(from_name.into()),
        from_address: Some("bob@example.com".into()),
        sent_at: Some(1_700_000_000),
        snippet: Some("...".into()),
        size_bytes: None,
        has_attachments: false,
        attachments: None,
        is_read: false,
        is_starred: false,
        addresses: vec![],
        text_body: Some(body.into()),
        sanitized_html: None,
        html_body: None,
        has_remote_content: false,
        references: vec![],
    })
    .unwrap();
}

/// Records every `GenerateRequest` the executor built, so a test can assert
/// on the FULL pipeline's assembled prompt — not just the pure builder
/// functions in Tasks 6-7. This is how the from_name exclusion and the
/// subject-line fixture are proven end to end.
struct CapturingBackend {
    captured: Mutex<Vec<GenerateRequest>>,
    reply: String,
}

impl CapturingBackend {
    fn new(reply: &str) -> Self {
        Self {
            captured: Mutex::new(Vec::new()),
            reply: reply.to_string(),
        }
    }
}

impl AiBackend for CapturingBackend {
    fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        Ok(vec![])
    }
    fn generate(
        &self,
        req: &GenerateRequest,
        on_delta: &mut dyn FnMut(&str) -> StreamControl,
    ) -> Result<GenerateSummary, AiError> {
        self.captured.lock().unwrap().push(req.clone());
        on_delta(&self.reply);
        Ok(GenerateSummary {
            tokens_in: Some(10),
            tokens_out: Some(5),
        })
    }
    fn embed(&self, _model: &str, _text: &str) -> Result<Vec<f32>, AiError> {
        // Not exercised by these injection-defense tests (they only ever
        // enqueue generate-shaped jobs); a fixed vector is enough to satisfy
        // the trait.
        Ok(vec![0.0])
    }
}

#[test]
fn sender_display_name_never_reaches_the_model() {
    let raw = include_str!("fixtures/injection/08_sender_display_name_injection.txt");
    let attack = raw.trim();
    let db = temp_db();
    seed_message_with(
        &db,
        "acc1:INBOX:1",
        "Hi",
        attack,
        "Just checking in, see you Friday.",
    );
    db.enqueue_ai_job(
        Some("acc1"),
        "summarize_message",
        &serde_json::json!({ "message_id": "acc1:INBOX:1" }).to_string(),
        0,
    )
    .unwrap();

    let mut backend = CapturingBackend::new("Short summary.");
    run_ai_jobs_with_backend(&db, &mut backend, &run_config()).unwrap();

    let captured = backend.captured.into_inner().unwrap();
    assert_eq!(captured.len(), 1);
    let req = &captured[0];
    assert!(
        !req.system.contains(attack),
        "from_name attack leaked into system: {}",
        req.system
    );
    assert!(
        !req.user.contains(attack),
        "from_name attack leaked into user — build_prompt must never read header.from_name: {}",
        req.user
    );
}

#[test]
fn subject_line_injection_is_quarantined_through_the_real_build_prompt_path() {
    let raw = include_str!("fixtures/injection/07_subject_line_injection.txt");
    let subject = raw.trim();
    let db = temp_db();
    seed_message_with(
        &db,
        "acc1:INBOX:2",
        subject,
        "Alice",
        "Please see the attached invoice.",
    );
    db.enqueue_ai_job(
        Some("acc1"),
        "summarize_message",
        &serde_json::json!({ "message_id": "acc1:INBOX:2" }).to_string(),
        0,
    )
    .unwrap();

    let mut backend = CapturingBackend::new("Short summary.");
    run_ai_jobs_with_backend(&db, &mut backend, &run_config()).unwrap();

    let captured = backend.captured.into_inner().unwrap();
    let req = &captured[0];
    assert_eq!(
        req.user.matches(defense::DATA_BEGIN).count(),
        1,
        "{}",
        req.user
    );
    assert_eq!(
        req.user.matches(defense::DATA_END).count(),
        1,
        "{}",
        req.user
    );
    assert!(
        req.user.contains(subject),
        "subject content should be preserved: {}",
        req.user
    );
    assert!(
        !req.system.contains("evil.example") && !req.system.to_lowercase().contains("override"),
        "system: {}",
        req.system
    );
}

#[test]
fn draft_reply_envelopes_the_original_message_and_keeps_instruction_outside() {
    let raw = include_str!("fixtures/injection/11_draft_exfiltration_url.txt");
    let db = temp_db();
    seed_message_with(&db, "acc1:INBOX:3", "Re: contract", "Carol", raw);
    db.enqueue_ai_job(
        Some("acc1"),
        "draft_reply",
        &serde_json::json!({ "message_id": "acc1:INBOX:3", "instruction": "Accept politely." })
            .to_string(),
        0,
    )
    .unwrap();

    let mut backend = CapturingBackend::new("Sounds good, thank you.");
    run_ai_jobs_with_backend(&db, &mut backend, &run_config()).unwrap();

    let captured = backend.captured.into_inner().unwrap();
    let req = &captured[0];
    assert_eq!(
        req.user.matches(defense::DATA_BEGIN).count(),
        1,
        "{}",
        req.user
    );
    assert_eq!(
        req.user.matches(defense::DATA_END).count(),
        1,
        "{}",
        req.user
    );

    // The user's own instruction must stay OUTSIDE the fence — it is
    // trusted, first-party guidance, not attacker content.
    let end = req.user.find(defense::DATA_END).unwrap();
    let instr = req
        .user
        .find("Instruction: Accept politely.")
        .expect("instruction present");
    assert!(
        instr > end,
        "instruction must be outside (after) the untrusted-content fence: {}",
        req.user
    );
}

// ---------------------------------------------------------------------------
// Live-Ollama smoke test (optional, NOT part of the automatic gate)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn live_ollama_corpus_smoke_test() {
    let Ok(url) = std::env::var("CASTELLUM_OLLAMA_URL") else {
        eprintln!("[ai_injection_defense] CASTELLUM_OLLAMA_URL not set — skipping");
        return;
    };
    let model = std::env::var("CASTELLUM_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.1:8b".into());

    use castellum_provider_ollama::{AiConfig, OllamaClient};
    let ai_config = AiConfig {
        base_url: url.clone(),
        request_timeout_secs: 60,
        connect_timeout_secs: 5,
    };
    let Ok(mut client) = OllamaClient::connect(&ai_config) else {
        eprintln!("[ai_injection_defense] endpoint not reachable — skipping");
        return;
    };
    let model_ok = client
        .list_models()
        .map(|ms| ms.iter().any(|m| m.name.starts_with(&model)))
        .unwrap_or(false);
    if !model_ok {
        eprintln!("[ai_injection_defense] model {model:?} not available — skipping");
        return;
    }

    let db = temp_db();
    let fixtures: &[(&str, &str)] = &[
        (
            "ignore_previous_instructions",
            include_str!("fixtures/injection/01_ignore_previous_instructions.txt"),
        ),
        (
            "exfiltrate_other_mail",
            include_str!("fixtures/injection/10_exfiltrate_other_mail.txt"),
        ),
        (
            "role_play_jailbreak",
            include_str!("fixtures/injection/13_role_play_jailbreak.txt"),
        ),
    ];
    for (name, raw) in fixtures {
        let msg_id = format!("acc1:INBOX:{name}");
        seed_message_with(&db, &msg_id, "Weekly update", "Bob", raw);
        db.enqueue_ai_job(
            Some("acc1"),
            "summarize_message",
            &serde_json::json!({ "message_id": msg_id }).to_string(),
            0,
        )
        .unwrap();
    }

    let mut cfg = run_config();
    cfg.model = model;
    let report = run_ai_jobs_with_backend(&db, &mut client, &cfg).expect("run");
    eprintln!("[ai_injection_defense] live corpus run: {report:?}");

    for (name, _) in fixtures {
        let msg_id = format!("acc1:INBOX:{name}");
        if let Some(out) = db
            .get_ai_output("message", &msg_id, "summarize_message")
            .unwrap()
        {
            eprintln!("[ai_injection_defense] {name} -> {:?}", out.text);
        }
    }
    // Best-effort: printed for a human to skim, not a hard assertion
    // against live model behaviour — no LLM output is deterministic enough
    // to pin in CI. The structural guarantees are already proven by Tasks
    // 6-8 without a live model.
}
