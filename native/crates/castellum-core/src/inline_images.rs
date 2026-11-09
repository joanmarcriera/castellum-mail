// Castellum - inline images in Markdown mail (MM-5b, slice 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// An image embedded in a Markdown draft is a `draft_attachments` row with
// `is_inline = 1` and a Content-ID the CORE generates (so the reference in the
// Markdown source is the `cid:` URL itself). The renderer admits `<img>` only
// for those IDs, and the SMTP builder sends the bytes as `multipart/related`
// (see `castellum_provider_smtp::build_rfc822_with`). No migration: the
// columns exist since 0030.
//
// Trust: the image type is decided from the BYTES (PNG, JPEG, GIF, WebP) and
// never from a declared type or extension; SVG, HTML or anything else is
// refused as inline (the compose preview serves these bytes to a WKWebView and
// recipients render them, so only rasters are ever embedded).

use crate::{CoreError, Profile};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use sha2::{Digest, Sha256};

/// Largest inline image (decimal MB), bounding base64 over the JSON ABI.
pub const INLINE_IMAGE_MAX_BYTES: usize = 10 * 1_000_000;

/// PNG / JPEG / GIF / WebP by magic bytes; `None` for anything else.
pub fn sniff_inline_image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// `img-<first 12 hex of sha256(bytes)>@castellum`: deterministic, so the same
/// bytes embed once; leaks no hostname, filename or time.
pub fn content_id_for(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("img-{hex}@castellum")
}

fn alt_for(alt: Option<&str>, filename: &str) -> String {
    let base = match alt.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => a.to_string(),
        None => filename
            .rsplit_once('.')
            .map_or(filename, |(stem, _)| stem)
            .to_string(),
    };
    base.chars()
        .filter(|c| !matches!(c, '[' | ']' | '\n' | '\r'))
        .collect::<String>()
        .trim()
        .to_string()
}

fn db_err(e: impl std::fmt::Display) -> CoreError {
    CoreError::Db(e.to_string())
}

impl Profile {
    /// Embed one image in a draft. `image_json` is `{filename, alt?,
    /// data_base64}`. Returns `{"attachment": DraftAttachmentMeta,
    /// "markdown": "![alt](cid:…)", "attachments": [...]}`. Refuses (writing
    /// nothing) with `CoreError::NonRetryable` whose message starts
    /// `not_an_image:` or `too_large:`. The same bytes twice give one row.
    pub fn embed_draft_image_json(
        &self,
        draft_id: i64,
        image_json: &str,
    ) -> Result<String, CoreError> {
        #[derive(serde::Deserialize)]
        struct NewImage {
            filename: String,
            #[serde(default)]
            alt: Option<String>,
            data_base64: String,
        }
        let img: NewImage =
            serde_json::from_str(image_json).map_err(|_| CoreError::InvalidArgument)?;
        let bytes = BASE64
            .decode(&img.data_base64)
            .map_err(|_| CoreError::InvalidArgument)?;
        if bytes.len() > INLINE_IMAGE_MAX_BYTES {
            return Err(CoreError::NonRetryable(format!(
                "too_large: an inline image may be at most {} MB",
                INLINE_IMAGE_MAX_BYTES / 1_000_000
            )));
        }
        let Some(content_type) = sniff_inline_image_type(&bytes) else {
            return Err(CoreError::NonRetryable(
                "not_an_image: only PNG, JPEG, GIF and WebP can be embedded".into(),
            ));
        };
        let cid = content_id_for(&bytes);
        let existing = self
            .db
            .draft_inline_attachment(draft_id, &cid)
            .map_err(db_err)?;
        let attachment = match existing {
            Some((meta, _)) => meta,
            None => {
                self.db
                    .add_draft_attachment(
                        draft_id,
                        &img.filename,
                        content_type,
                        true,
                        Some(&cid),
                        &bytes,
                    )
                    .map_err(db_err)?;
                self.db
                    .draft_inline_attachment(draft_id, &cid)
                    .map_err(db_err)?
                    .map(|(m, _)| m)
                    .ok_or_else(|| CoreError::Db("embedded image vanished".into()))?
            }
        };
        let attachments = self.db.list_draft_attachments(draft_id).map_err(db_err)?;
        let markdown = format!(
            "![{}](cid:{cid})",
            alt_for(img.alt.as_deref(), &img.filename)
        );
        serde_json::to_string(&serde_json::json!({
            "attachment": attachment,
            "markdown": markdown,
            "attachments": attachments,
        }))
        .map_err(|_| CoreError::Io)
    }

    /// Make an existing draft attachment inline (`inline = true`) or a regular
    /// attachment again (`false`) (slice 3). Returns `{"attachments": [...],
    /// "markdown": "![name](cid:…)" | null}`; the Markdown is the reference to insert
    /// when it became inline. Becoming inline needs PNG/JPEG/GIF/WebP bytes within
    /// 10 MB (`not_an_image:` / `too_large:` refusals, nothing changes); the row's content
    /// type is corrected to the sniffed one. If the same bytes are already embedded as
    /// another inline row, that row is reused and this regular duplicate is removed.
    /// Refused unless the draft is still `draft`.
    pub fn set_draft_attachment_inline_json(
        &self,
        draft_id: i64,
        attachment_id: i64,
        inline: bool,
    ) -> Result<String, CoreError> {
        let rows = self.db.list_draft_attachments(draft_id).map_err(db_err)?;
        let Some(row) = rows.iter().find(|m| m.id == attachment_id) else {
            return Err(CoreError::InvalidArgument);
        };
        let mut markdown: Option<String> = None;
        if inline {
            let bytes = self
                .db
                .draft_attachment_bytes(attachment_id)
                .map_err(db_err)?
                .ok_or(CoreError::InvalidArgument)?;
            if bytes.len() > INLINE_IMAGE_MAX_BYTES {
                return Err(CoreError::NonRetryable(format!(
                    "too_large: an inline image may be at most {} MB",
                    INLINE_IMAGE_MAX_BYTES / 1_000_000
                )));
            }
            let Some(content_type) = sniff_inline_image_type(&bytes) else {
                return Err(CoreError::NonRetryable(
                    "not_an_image: only PNG, JPEG, GIF and WebP can be embedded".into(),
                ));
            };
            let cid = content_id_for(&bytes);
            markdown = Some(format!("![{}](cid:{cid})", alt_for(None, &row.filename)));
            let duplicate = rows.iter().any(|m| {
                m.id != attachment_id && m.is_inline && m.content_id.as_deref() == Some(&cid)
            });
            if duplicate {
                // The same picture is already embedded: reuse that row, drop this copy.
                self.db
                    .remove_draft_attachment(draft_id, attachment_id)
                    .map_err(db_err)?;
            } else {
                self.db
                    .set_draft_attachment_inline(
                        draft_id,
                        attachment_id,
                        Some((&cid, content_type)),
                    )
                    .map_err(db_err)?;
            }
        } else {
            self.db
                .set_draft_attachment_inline(draft_id, attachment_id, None)
                .map_err(db_err)?;
        }
        let attachments = self.db.list_draft_attachments(draft_id).map_err(db_err)?;
        serde_json::to_string(&serde_json::json!({
            "attachments": attachments,
            "markdown": markdown,
        }))
        .map_err(|_| CoreError::Io)
    }

    /// Render `markdown` for `draft_id`: images whose URL is `cid:<id>` of one
    /// of the draft's inline attachments (with a safe Content-ID) become
    /// `<img>`; every other image is dropped as before. A draft with none
    /// renders exactly what `compose::render_markdown_themed` returns.
    pub fn render_markdown_for_draft(
        &self,
        draft_id: i64,
        markdown: &str,
        theme: &str,
    ) -> Result<String, CoreError> {
        let cids: Vec<String> = self
            .db
            .list_draft_attachments(draft_id)
            .map_err(db_err)?
            .into_iter()
            .filter(|m| m.is_inline)
            .filter_map(|m| m.content_id)
            .filter(|c| castellum_mail::is_safe_content_id(c))
            .collect();
        let refs: Vec<&str> = cids.iter().map(String::as_str).collect();
        Ok(castellum_mail::render_markdown_to_html_with_images(
            markdown,
            castellum_mail::MarkdownTheme::from_name(theme),
            &refs,
        ))
    }

    /// `{"content_type", "data_base64"}` of the inline attachment of
    /// `draft_id` with this Content-ID, for the compose preview. The content
    /// type is re-sniffed from the bytes: a non-raster part (e.g. a forwarded
    /// SVG) is an error, never served.
    pub fn draft_inline_image_json(
        &self,
        draft_id: i64,
        content_id: &str,
    ) -> Result<String, CoreError> {
        let Some((_, bytes)) = self
            .db
            .draft_inline_attachment(draft_id, content_id)
            .map_err(db_err)?
        else {
            return Err(CoreError::InvalidArgument);
        };
        let Some(content_type) = sniff_inline_image_type(&bytes) else {
            return Err(CoreError::InvalidArgument);
        };
        serde_json::to_string(&serde_json::json!({
            "content_type": content_type,
            "data_base64": BASE64.encode(&bytes),
        }))
        .map_err(|_| CoreError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_profile;
    use castellum_models::NewDraft;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];

    fn profile(tag: &str) -> Profile {
        let dir = castellum_testutil::scratch_root()
            .join(format!("castellum-inline-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        open_profile(&dir).unwrap()
    }

    fn draft(p: &Profile) -> i64 {
        p.db.upsert_account(&castellum_models::Account {
            id: "a".into(),
            email: "a@x.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        p.db.save_draft(&NewDraft {
            id: None,
            account_id: "a".into(),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: String::new(),
            body_text: String::new(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        })
        .unwrap()
    }

    fn json(name: &str, bytes: &[u8]) -> String {
        serde_json::json!({"filename": name, "data_base64": BASE64.encode(bytes)}).to_string()
    }

    #[test]
    fn sniffing_accepts_four_rasters_and_nothing_else() {
        assert_eq!(sniff_inline_image_type(PNG), Some("image/png"));
        assert_eq!(
            sniff_inline_image_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_inline_image_type(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_inline_image_type(b"GIF87a...."), Some("image/gif"));
        assert_eq!(
            sniff_inline_image_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        for not in [
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"[..],
            b"<html><script>alert(1)</script>",
            b"%PDF-1.7",
            b"",
            b"RIFF\x00\x00\x00\x00WAVEfmt ",
        ] {
            assert_eq!(sniff_inline_image_type(not), None, "{not:?}");
        }
    }

    #[test]
    fn content_id_is_deterministic_and_safe() {
        let a = content_id_for(PNG);
        assert_eq!(a, content_id_for(PNG));
        assert_ne!(a, content_id_for(b"other"));
        assert!(castellum_mail::is_safe_content_id(&a));
        assert!(a.starts_with("img-") && a.ends_with("@castellum"));
    }

    #[test]
    fn embedding_stores_an_inline_row_and_returns_the_markdown() {
        let p = profile("embed");
        let id = draft(&p);
        let out: serde_json::Value = serde_json::from_str(
            &p.embed_draft_image_json(id, &json("Screen Shot.png", PNG))
                .unwrap(),
        )
        .unwrap();
        let cid = content_id_for(PNG);
        assert_eq!(out["markdown"], format!("![Screen Shot](cid:{cid})"));
        assert_eq!(out["attachment"]["is_inline"], true);
        assert_eq!(out["attachment"]["content_id"], cid);
        assert_eq!(out["attachment"]["content_type"], "image/png");
        assert_eq!(out["attachments"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_same_bytes_twice_give_one_row_and_the_same_markdown() {
        let p = profile("dedupe");
        let id = draft(&p);
        let a = p.embed_draft_image_json(id, &json("a.png", PNG)).unwrap();
        let b = p.embed_draft_image_json(id, &json("b.png", PNG)).unwrap();
        let (a, b): (serde_json::Value, serde_json::Value) = (
            serde_json::from_str(&a).unwrap(),
            serde_json::from_str(&b).unwrap(),
        );
        assert_eq!(b["attachments"].as_array().unwrap().len(), 1);
        assert_eq!(a["attachment"]["id"], b["attachment"]["id"]);
    }

    #[test]
    fn refusals_write_nothing() {
        let p = profile("refuse");
        let id = draft(&p);
        let svg = p
            .embed_draft_image_json(id, &json("x.png", b"<svg onload=alert(1)></svg>"))
            .unwrap_err();
        assert!(
            matches!(&svg, CoreError::NonRetryable(m) if m.starts_with("not_an_image:")),
            "{svg:?}"
        );
        let mut big = PNG.to_vec();
        big.resize(INLINE_IMAGE_MAX_BYTES + 1, 0);
        let too_big = p
            .embed_draft_image_json(id, &json("big.png", &big))
            .unwrap_err();
        assert!(
            matches!(&too_big, CoreError::NonRetryable(m) if m.starts_with("too_large:")),
            "{too_big:?}"
        );
        assert!(p.db.list_draft_attachments(id).unwrap().is_empty());
        assert!(matches!(
            p.embed_draft_image_json(id, "not json"),
            Err(CoreError::InvalidArgument)
        ));
    }

    #[test]
    fn render_for_draft_shows_the_image_and_equals_the_stateless_render_without_one() {
        let p = profile("render");
        let id = draft(&p);
        let md = "Hello ![shot](cid:img-000000000000@castellum) world";
        assert_eq!(
            p.render_markdown_for_draft(id, md, "clean").unwrap(),
            crate::compose::render_markdown_themed(md, "clean")
        );
        let out: serde_json::Value =
            serde_json::from_str(&p.embed_draft_image_json(id, &json("a.png", PNG)).unwrap())
                .unwrap();
        let md = format!("Hi {}", out["markdown"].as_str().unwrap());
        let html = p.render_markdown_for_draft(id, &md, "plain").unwrap();
        assert!(html.contains("<img"), "{html}");
        assert!(html.contains(&content_id_for(PNG)), "{html}");
    }

    #[test]
    fn an_unsafe_forwarded_content_id_is_ignored_by_the_renderer() {
        let p = profile("unsafe");
        let id = draft(&p);
        p.db.add_draft_attachment(id, "x.png", "image/png", true, Some("a\"b onload=x"), PNG)
            .unwrap();
        let html = p
            .render_markdown_for_draft(id, "![x](cid:a\"b onload=x)", "plain")
            .unwrap();
        assert!(!html.contains("<img"), "{html}");
    }

    #[test]
    fn the_preview_bytes_are_resniffed_and_a_non_raster_part_is_refused() {
        let p = profile("preview");
        let id = draft(&p);
        let cid = content_id_for(PNG);
        p.embed_draft_image_json(id, &json("a.png", PNG)).unwrap();
        let out: serde_json::Value =
            serde_json::from_str(&p.draft_inline_image_json(id, &cid).unwrap()).unwrap();
        assert_eq!(out["content_type"], "image/png");
        assert_eq!(
            BASE64.decode(out["data_base64"].as_str().unwrap()).unwrap(),
            PNG
        );
        // A forwarded inline SVG (declared image/svg+xml) is never served.
        p.db.add_draft_attachment(
            id,
            "v.svg",
            "image/svg+xml",
            true,
            Some("svg1@x"),
            b"<svg/>",
        )
        .unwrap();
        assert!(p.draft_inline_image_json(id, "svg1@x").is_err());
        assert!(p.draft_inline_image_json(id, "missing@x").is_err());
    }

    fn add_regular(p: &Profile, id: i64, name: &str, ct: &str, bytes: &[u8]) -> i64 {
        p.db.add_draft_attachment(id, name, ct, false, None, bytes)
            .unwrap()
    }

    #[test]
    fn a_regular_image_attachment_can_be_made_inline_and_back() {
        let p = profile("flip");
        let id = draft(&p);
        let att = add_regular(&p, id, "photo.png", "application/octet-stream", PNG);
        let out: serde_json::Value =
            serde_json::from_str(&p.set_draft_attachment_inline_json(id, att, true).unwrap())
                .unwrap();
        let cid = content_id_for(PNG);
        assert_eq!(out["markdown"], format!("![photo](cid:{cid})"));
        let row = &out["attachments"][0];
        assert_eq!(row["is_inline"], true);
        assert_eq!(row["content_id"], cid);
        assert_eq!(
            row["content_type"], "image/png",
            "corrected to the sniffed type"
        );

        let back: serde_json::Value =
            serde_json::from_str(&p.set_draft_attachment_inline_json(id, att, false).unwrap())
                .unwrap();
        assert!(back["markdown"].is_null());
        assert_eq!(back["attachments"][0]["is_inline"], false);
        assert!(back["attachments"][0]["content_id"].is_null());
        assert_eq!(back["attachments"][0]["content_type"], "image/png");
    }

    #[test]
    fn a_non_image_or_oversize_attachment_stays_regular() {
        let p = profile("flip-refuse");
        let id = draft(&p);
        let pdf = add_regular(&p, id, "a.pdf", "application/pdf", b"%PDF-1.7 ...");
        let err = p
            .set_draft_attachment_inline_json(id, pdf, true)
            .unwrap_err();
        assert!(
            matches!(&err, CoreError::NonRetryable(m) if m.starts_with("not_an_image:")),
            "{err:?}"
        );
        let mut big = PNG.to_vec();
        big.resize(INLINE_IMAGE_MAX_BYTES + 1, 0);
        let huge = add_regular(&p, id, "big.png", "image/png", &big);
        let err = p
            .set_draft_attachment_inline_json(id, huge, true)
            .unwrap_err();
        assert!(
            matches!(&err, CoreError::NonRetryable(m) if m.starts_with("too_large:")),
            "{err:?}"
        );
        let rows = p.db.list_draft_attachments(id).unwrap();
        assert!(rows.iter().all(|r| !r.is_inline && r.content_id.is_none()));
    }

    #[test]
    fn making_a_duplicate_inline_reuses_the_embedded_row_and_drops_the_copy() {
        let p = profile("flip-dup");
        let id = draft(&p);
        p.embed_draft_image_json(id, &json("one.png", PNG)).unwrap();
        let copy = add_regular(&p, id, "copy.png", "image/png", PNG);
        let out: serde_json::Value =
            serde_json::from_str(&p.set_draft_attachment_inline_json(id, copy, true).unwrap())
                .unwrap();
        assert_eq!(
            out["attachments"].as_array().unwrap().len(),
            1,
            "one row left"
        );
        assert_eq!(out["attachments"][0]["filename"], "one.png");
        assert!(out["markdown"]
            .as_str()
            .unwrap()
            .contains(&content_id_for(PNG)));
    }

    #[test]
    fn an_unknown_attachment_is_refused() {
        let p = profile("flip-unknown");
        let id = draft(&p);
        assert!(matches!(
            p.set_draft_attachment_inline_json(id, 9_999, true),
            Err(CoreError::InvalidArgument)
        ));
    }

    #[test]
    fn another_drafts_attachment_cannot_be_flipped() {
        let p = profile("flip-other");
        let a = draft(&p);
        let b =
            p.db.save_draft(&NewDraft {
                id: None,
                account_id: "a".into(),
                to: vec![],
                cc: vec![],
                bcc: vec![],
                subject: String::new(),
                body_text: String::new(),
                body_html: None,
                from_email: None,
                from_name: None,
                in_reply_to: None,
                references_header: None,
            })
            .unwrap();
        let att = add_regular(&p, a, "a.png", "image/png", PNG);
        assert!(matches!(
            p.set_draft_attachment_inline_json(b, att, true),
            Err(CoreError::InvalidArgument)
        ));
    }
}
