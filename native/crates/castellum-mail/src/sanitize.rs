// Castellum - the single audited HTML sanitization path (Wall 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Email bodies are ADVERSARIAL HTML. Policy = Electron DOMPurify parity
// (src/renderer/utils/email-html.ts at tag `electron-final`) with two
// justified divergences (inline `style` allowed; no link target/rel rewrite)
// — both backstopped by the WKWebView wall (JS off, all navigation
// cancelled, WKContentRuleList network blocking). See the design spec.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// Tags allowed in received mail. Generous structural set (email is table
/// soup) but NOTHING that executes, embeds, navigates, or submits.
const ALLOWED_TAGS: &[&str] = &[
    "a",
    "b",
    "blockquote",
    "br",
    "caption",
    "code",
    "col",
    "colgroup",
    "del",
    "div",
    "em",
    "figcaption",
    "figure",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "s",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];

/// Attributes allowed on any tag. `style` is allowed (divergence from
/// Electron) because the WKContentRuleList blocks CSS `url()` fetches at the
/// network layer; see the spec.
const GENERIC_ATTRS: &[&str] = &[
    "align",
    "bgcolor",
    "border",
    "cellpadding",
    "cellspacing",
    "class",
    "color",
    "dir",
    "height",
    "style",
    "valign",
    "width",
];

const DATA_IMAGE_PREFIXES: &[&str] = &[
    "data:image/png",
    "data:image/gif",
    "data:image/jpeg",
    "data:image/webp",
];

/// Sanitized output plus whether any remote (http/https or //) `src` survived,
/// so the UI knows to show the "Load remote images" banner without scanning
/// HTML in Swift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedHtml {
    pub html: String,
    pub has_remote_content: bool,
}

pub fn sanitize_email_html(html: &str) -> SanitizedHtml {
    if html.is_empty() {
        return SanitizedHtml {
            html: String::new(),
            has_remote_content: false,
        };
    }

    let mut tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::new();
    tag_attributes.insert("a", ["href", "name", "title"].into_iter().collect());
    tag_attributes.insert("img", ["src", "alt", "title"].into_iter().collect());
    tag_attributes.insert("td", ["colspan", "rowspan"].into_iter().collect());
    tag_attributes.insert("th", ["colspan", "rowspan"].into_iter().collect());
    tag_attributes.insert("col", ["span"].into_iter().collect());
    tag_attributes.insert("colgroup", ["span"].into_iter().collect());

    let clean = ammonia::Builder::default()
        .tags(ALLOWED_TAGS.iter().copied().collect())
        .generic_attributes(GENERIC_ATTRS.iter().copied().collect())
        .tag_attributes(tag_attributes)
        // Drop the entire content of <style> and <script> blocks (not just the
        // tags) so body{background:url(...)} CSS text cannot leak as text nodes.
        .clean_content_tags(["script", "style"].into_iter().collect())
        // `data` must be listed so data:image/* can survive; the attribute
        // filter below rejects every non-image data: URI.
        .url_schemes(
            ["http", "https", "mailto", "tel", "cid", "data"]
                .into_iter()
                .collect(),
        )
        .attribute_filter(|_element, attribute, value| {
            let is_url_attr = attribute == "src" || attribute == "href";
            if is_url_attr {
                let v = value.trim().to_ascii_lowercase();
                if v.starts_with("data:") && !DATA_IMAGE_PREFIXES.iter().any(|p| v.starts_with(p)) {
                    return None; // strip non-image (incl. svg+xml) data: URIs
                }
            }
            Some(Cow::Borrowed(value))
        })
        .clean(html)
        .to_string();

    let has_remote_content = detect_remote_content(&clean);
    SanitizedHtml {
        html: clean,
        has_remote_content,
    }
}

/// Ammonia normalizes attributes to `name="value"` with double quotes, so a
/// plain substring scan of the SANITIZED output is reliable here. CSS `url()`
/// inside allowed `style` attributes counts too: with "always load remote
/// images" enabled, a background-image tracking pixel would otherwise load
/// without the banner ever having been shown.
fn detect_remote_content(sanitized: &str) -> bool {
    let lower = sanitized.to_ascii_lowercase();
    if ["src=\"http://", "src=\"https://", "src=\"//"]
        .iter()
        .any(|p| lower.contains(p))
    {
        return true;
    }
    // Every `url(` occurrence, tolerating whitespace and CSS quote styles
    // (', ", and ammonia's &quot; encoding inside attribute values).
    lower.match_indices("url(").any(|(i, _)| {
        let rest = lower[i + 4..]
            .trim_start_matches(char::is_whitespace)
            .trim_start_matches("&quot;")
            .trim_start_matches(['\'', '"'])
            .trim_start_matches(char::is_whitespace);
        rest.starts_with("http://") || rest.starts_with("https://") || rest.starts_with("//")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(input: &str) -> String {
        sanitize_email_html(input).html
    }

    // --- active content is removed -----------------------------------------

    #[test]
    fn strips_script_tags_and_contents() {
        let out = html("<p>hi</p><script>alert(1)</script>");
        assert!(!out.contains("script"));
        assert!(!out.contains("alert"));
        assert!(out.contains("hi"));
    }

    #[test]
    fn strips_event_handlers() {
        let out = html(r#"<img src="https://a/b.png" onerror="alert(1)">"#);
        assert!(!out.contains("onerror"));
        assert!(!out.contains("alert"));
    }

    #[test]
    fn strips_javascript_uris() {
        let out = html(r#"<a href="javascript:alert(1)">x</a>"#);
        assert!(!out.contains("javascript:"));
    }

    #[test]
    fn strips_embedding_and_form_elements() {
        for bad in [
            "<iframe src='https://x'></iframe>",
            "<object data='x'></object>",
            "<embed src='x'>",
            "<form action='https://x'><input name='a'></form>",
            "<meta http-equiv='refresh' content='0;url=https://x'>",
            "<base href='https://x/'>",
            "<map name='m'><area href='https://x'></map>",
            "<link rel='stylesheet' href='https://x'>",
        ] {
            let out = html(&format!("<p>keep</p>{bad}"));
            for token in [
                "iframe", "object", "embed", "<form", "<input", "<meta", "<base", "<map", "<area",
                "<link",
            ] {
                assert!(!out.contains(token), "{bad} leaked {token}: {out}");
            }
            assert!(out.contains("keep"));
        }
    }

    #[test]
    fn strips_tracking_attributes() {
        let out = html(
            r#"<a href="https://x" ping="https://t">x</a><img src="https://a/b.png" srcset="https://t 1x" lowsrc="https://t"><td background="https://t">y</td>"#,
        );
        for attr in ["ping", "srcset", "lowsrc", "background="] {
            assert!(!out.contains(attr), "leaked {attr}: {out}");
        }
    }

    #[test]
    fn strips_style_elements_but_keeps_style_attributes() {
        let out =
            html(r#"<style>body{background:url(https://t)}</style><p style="color:#333">x</p>"#);
        assert!(!out.contains("<style"));
        assert!(!out.contains("background:url"));
        assert!(out.contains(r#"style="color:#333""#));
    }

    // --- URI scheme allowlist -----------------------------------------------

    #[test]
    fn allows_safe_schemes() {
        let out = html(
            r#"<a href="https://x">a</a><a href="mailto:a@b">b</a><a href="tel:+441223">c</a><img src="cid:part1"><img src="data:image/png;base64,iVBORw0KGgo=">"#,
        );
        for good in [
            "https://x",
            "mailto:a@b",
            "tel:+441223",
            "cid:part1",
            "data:image/png",
        ] {
            assert!(out.contains(good), "lost {good}: {out}");
        }
    }

    #[test]
    fn rejects_non_image_and_svg_data_uris() {
        let out = html(
            r#"<a href="data:text/html;base64,PHNjcmlwdD4=">x</a><img src="data:image/svg+xml;base64,PHN2Zz4=">"#,
        );
        assert!(!out.contains("data:text/html"));
        assert!(!out.contains("svg+xml"));
    }

    // --- fidelity: newsletter table soup survives --------------------------

    #[test]
    fn keeps_newsletter_structure() {
        let newsletter = r##"<table width="600" cellpadding="0" cellspacing="0" border="0" align="center"><tbody><tr><td bgcolor="#ffffff" style="padding:20px"><h1>News</h1><p>Body <strong>bold</strong> <a href="https://example.com">link</a></p><img src="https://example.com/hero.png" width="560" alt="hero"></td></tr></tbody></table>"##;
        let out = html(newsletter);
        for keep in [
            "<table",
            "<td",
            "width=\"600\"",
            "bgcolor=",
            "style=",
            "<h1>",
            "https://example.com/hero.png",
            "alt=\"hero\"",
        ] {
            assert!(out.contains(keep), "lost {keep}: {out}");
        }
    }

    // --- remote-content flag ------------------------------------------------

    #[test]
    fn flags_remote_content() {
        assert!(sanitize_email_html(r#"<img src="https://t/p.gif">"#).has_remote_content);
        assert!(sanitize_email_html(r#"<img src="//t/p.gif">"#).has_remote_content);
        assert!(!sanitize_email_html(r#"<img src="cid:x"><p>text</p>"#).has_remote_content);
        assert!(
            !sanitize_email_html(r#"<img src="data:image/png;base64,AA==">"#).has_remote_content
        );
    }

    #[test]
    fn flags_remote_css_url_in_style_attributes() {
        for input in [
            r#"<div style="background: url(https://t/p.gif)">x</div>"#,
            r#"<div style="background: url( 'https://t/p.gif' )">x</div>"#,
            r#"<div style='background-image: url("//t/p.gif")'>x</div>"#,
            r#"<div style="background:url(http://t/p.gif)">x</div>"#,
        ] {
            let out = sanitize_email_html(input);
            assert!(
                out.has_remote_content,
                "input {input:?} not flagged: {}",
                out.html
            );
        }
        // Local/relative url() values must not trip the banner.
        let out = sanitize_email_html(r#"<div style="background: url(cid:logo)">x</div>"#);
        assert!(!out.has_remote_content, "cid url flagged: {}", out.html);
    }

    #[test]
    fn empty_input_is_empty_and_not_remote() {
        let out = sanitize_email_html("");
        assert_eq!(out.html, "");
        assert!(!out.has_remote_content);
    }
}
