// Castellum - Markdown compose renderer (S46).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Renders a compose window's Markdown SOURCE (the same string `NewDraft::body_text`
// stores) into sanitized HTML for two consumers that must never disagree: the
// compose window's live preview (`castellum_render_markdown_preview`, via
// `castellum_core::compose::render_markdown_preview`) and the `text/html` MIME
// part built at send time (`Draft::body_html`, consumed by
// `castellum_provider_smtp::build_rfc822`). Calling this twice on the same
// input must always produce the same output — that is the whole point of
// having one renderer instead of two (see the S46 plan's Assumptions #3).

use crate::code_highlight;
use pulldown_cmark::{html, CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// Tags allowed in compose-rendered HTML. Deliberately narrower than
/// `sanitize::ALLOWED_TAGS` (incoming mail): no `img` at all — "no remote
/// assets pulled at compose time" is enforced by never emitting the tag, not
/// by a runtime remote-content policy like the reading pane's — and no
/// `style`/`script` survive either bucket.
const ALLOWED_TAGS: &[&str] = &[
    "p",
    "br",
    "strong",
    "em",
    "del",
    "code",
    "pre",
    "blockquote",
    "ul",
    "ol",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "a",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    // Only for syntax-highlight tokens; `style` is allowlisted to the fixed
    // `code_highlight::token_styles()` values (see the attribute filters).
    "span",
];

/// Render `markdown` (CommonMark + GFM tables/strikethrough) to sanitized
/// HTML. Never panics on any input, including malformed/adversarial byte
/// sequences: `pulldown_cmark::Parser` is a pure state machine over `&str`
/// (already valid UTF-8 by Rust's type system) that degrades unrecognised
/// syntax to literal text rather than erroring.
pub fn render_markdown_to_html(markdown: &str) -> String {
    render_markdown_to_html_themed(markdown, MarkdownTheme::Plain)
}

/// The visual style of the HTML alternative (MM-5). Mail clients drop
/// `<style>` blocks and the sanitiser above strips every `style` attribute the
/// Markdown could carry except the four fixed highlight-token values on
/// `span` (see `code_highlight::token_styles`), so a theme is applied AFTER sanitising by adding fixed,
/// hard-coded inline styles to tags we know the sanitiser left attribute-free.
/// No user text ever reaches a style value. `Plain` adds nothing (the S46 output,
/// byte for byte); an unknown name maps to `Plain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarkdownTheme {
    #[default]
    Plain,
    Clean,
    Serif,
    Compact,
}

impl MarkdownTheme {
    pub const ALL: [MarkdownTheme; 4] = [
        MarkdownTheme::Plain,
        MarkdownTheme::Clean,
        MarkdownTheme::Serif,
        MarkdownTheme::Compact,
    ];

    pub fn from_name(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|t| t.name().eq_ignore_ascii_case(name.trim()))
            .unwrap_or_default()
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Clean => "clean",
            Self::Serif => "serif",
            Self::Compact => "compact",
        }
    }

    /// (wrapper style, per-tag styles). Empty for `Plain`.
    fn styles(self) -> (String, Vec<(&'static str, String)>) {
        const MONO: &str = "font-family:Menlo,Consolas,'Courier New',monospace";
        let (font, size, line, gap) = match self {
            Self::Plain => return (String::new(), Vec::new()),
            Self::Clean => (
                "-apple-system,'Helvetica Neue',Arial,sans-serif",
                "14px",
                "1.5",
                "0 0 10px",
            ),
            Self::Serif => (
                "Georgia,'Times New Roman',serif",
                "15px",
                "1.55",
                "0 0 12px",
            ),
            Self::Compact => (
                "-apple-system,'Helvetica Neue',Arial,sans-serif",
                "13px",
                "1.35",
                "0 0 6px",
            ),
        };
        // Explicit white background: the text colours below are fixed dark
        // values, so without a matching background they are unreadable in a
        // dark-mode pane (compose preview) or dark-mode recipient client.
        let wrapper = format!(
            "font-family:{font};font-size:{size};line-height:{line};color:#1d1d1f;background:#ffffff;padding:8px"
        );
        let block = format!("margin:{gap}");
        let tags = vec![
            ("p", block.clone()),
            ("ul", block.clone()),
            ("ol", block.clone()),
            (
                "h1",
                format!("font-size:1.6em;line-height:1.25;margin:{gap}"),
            ),
            (
                "h2",
                format!("font-size:1.35em;line-height:1.3;margin:{gap}"),
            ),
            (
                "h3",
                format!("font-size:1.15em;line-height:1.3;margin:{gap}"),
            ),
            ("h4", format!("font-size:1em;margin:{gap}")),
            ("h5", format!("font-size:1em;margin:{gap}")),
            ("h6", format!("font-size:1em;margin:{gap}")),
            (
                "blockquote",
                format!(
                    "margin:{gap};padding:0 0 0 12px;border-left:3px solid #d0d0d5;color:#555555"
                ),
            ),
            ("table", format!("border-collapse:collapse;margin:{gap}")),
            (
                "th",
                "border:1px solid #d0d0d5;padding:4px 8px;background:#f5f5f7;text-align:left"
                    .to_string(),
            ),
            ("td", "border:1px solid #d0d0d5;padding:4px 8px".to_string()),
            (
                "hr",
                "border:0;border-top:1px solid #d0d0d5;margin:12px 0".to_string(),
            ),
            ("a", "color:#0a66c2".to_string()),
            (
                "code",
                format!(
                    "{MONO};font-size:0.92em;background:#f5f5f7;padding:1px 4px;border-radius:3px"
                ),
            ),
        ];
        (wrapper, tags)
    }
}

const PRE_STYLE: &str =
    "background:#f5f5f7;padding:10px 12px;border-radius:6px;overflow:auto;margin:0 0 10px";
const PRE_CODE_STYLE: &str =
    "font-family:Menlo,Consolas,'Courier New',monospace;font-size:0.92em;background:none;padding:0";

/// `render_markdown_to_html` with a theme (see [`MarkdownTheme`]).
pub fn render_markdown_to_html_themed(markdown: &str, theme: MarkdownTheme) -> String {
    let html = render_plain(markdown);
    apply_theme(&html, theme)
}

/// The one `style` an inline image carries, in every theme (T8): retina
/// screenshots would otherwise overflow the recipient's window. A constant, so
/// no user text reaches a style value.
pub const INLINE_IMG_STYLE: &str = "max-width:100%;height:auto";

/// True for a Content-ID that is safe to place unencoded in a `cid:` URL:
/// 1..=128 characters of `[A-Za-z0-9._@-]`.
pub fn is_safe_content_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'-'))
}

/// [`render_markdown_to_html_themed`], admitting `<img src="cid:ID">` for each
/// Content-ID in `inline_cids` (embedded images of the same draft, MM-5b).
/// Every other image (remote, `data:`, unknown `cid:`) is dropped, as before.
/// Unsafe Content-IDs in the set are ignored. With no usable ID this IS
/// `render_markdown_to_html_themed`, byte for byte (T5).
pub fn render_markdown_to_html_with_images(
    markdown: &str,
    theme: MarkdownTheme,
    inline_cids: &[&str],
) -> String {
    let cids: HashSet<&str> = inline_cids
        .iter()
        .copied()
        .filter(|c| is_safe_content_id(c))
        .collect();
    if cids.is_empty() {
        return render_markdown_to_html_themed(markdown, theme);
    }
    let html = render_with_images(markdown, &cids);
    apply_theme_with(&html, theme, &cids)
}

/// Content-IDs referenced as `src="cid:…"` in compose-rendered HTML, in order
/// of first use, de-duplicated. The renderer is the only writer of that shape,
/// so a plain scan is enough (no HTML parser).
pub fn referenced_cids(html: &str) -> Vec<String> {
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

/// Adds the theme's fixed inline styles to the sanitiser's output and wraps it.
fn apply_theme(sanitized: &str, theme: MarkdownTheme) -> String {
    apply_theme_with(sanitized, theme, &HashSet::new())
}

fn apply_theme_with(sanitized: &str, theme: MarkdownTheme, cids: &HashSet<&str>) -> String {
    let (wrapper, tags) = theme.styles();
    if wrapper.is_empty() || sanitized.is_empty() {
        return sanitized.to_string();
    }
    let mut html = sanitized.to_string();
    // A fenced/indented block is `<pre><code>`: style the pre, and reset the
    // inline-code look on the inner code so the block isn't double-padded.
    html = html.replace(
        "<pre><code>",
        &format!("<pre style=\"{PRE_STYLE}\"><code style=\"{PRE_CODE_STYLE}\">"),
    );
    for (tag, style) in &tags {
        html = html.replace(&format!("<{tag}>"), &format!("<{tag} style=\"{style}\">"));
        if *tag == "a" {
            // The sanitiser emits `<a href=... rel=...>`: attributes follow.
            html = html.replace("<a href=", &format!("<a style=\"{style}\" href="));
        }
    }
    let themed = format!("<div style=\"{wrapper}\">{html}</div>");
    // Defence in depth: theming is string surgery on serialised HTML. It is
    // safe today because the HTML serialiser escapes `<`/`>` inside attribute
    // values, but a dependency change must not be able to turn a hostile href
    // into live markup. So re-sanitise the result with a policy that admits
    // ONLY our own known style values; if that changes a single byte, the
    // theme is dropped and the plain (already sanitised) HTML is returned.
    if verified_theme_output(&themed, &wrapper, &tags, cids) {
        themed
    } else {
        sanitized.to_string()
    }
}

/// True when `themed` survives a strict re-sanitise unchanged: allowed tags
/// only, `style` only with a value we generate, `href` only on `a`, plus the
/// wrapper `div`.
fn verified_theme_output(
    themed: &str,
    wrapper: &str,
    tags: &[(&'static str, String)],
    cids: &HashSet<&str>,
) -> bool {
    let mut known_styles: HashSet<String> = tags.iter().map(|(_, style)| style.clone()).collect();
    known_styles.insert(wrapper.to_string());
    known_styles.insert(PRE_STYLE.to_string());
    known_styles.insert(PRE_CODE_STYLE.to_string());
    known_styles.extend(code_highlight::token_styles().map(String::from));
    let with_images = !cids.is_empty();
    if with_images {
        known_styles.insert(INLINE_IMG_STYLE.to_string());
    }
    let cid_srcs: HashSet<String> = cids.iter().map(|c| format!("cid:{c}")).collect();

    let mut allowed_tags: HashSet<&str> = ALLOWED_TAGS.iter().copied().collect();
    allowed_tags.insert("div");
    let mut tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::new();
    for tag in allowed_tags.iter() {
        tag_attributes.insert(tag, ["style"].into_iter().collect());
    }
    tag_attributes.insert("a", ["href", "style"].into_iter().collect());
    let mut schemes: HashSet<&str> = ["http", "https", "mailto"].into_iter().collect();
    if with_images {
        allowed_tags.insert("img");
        tag_attributes.insert("img", ["src", "alt", "style"].into_iter().collect());
        schemes.insert("cid");
    }

    let cleaned = ammonia::Builder::default()
        .tags(allowed_tags)
        .generic_attributes(HashSet::new())
        .tag_attributes(tag_attributes)
        .url_schemes(schemes)
        .attribute_filter(move |element, attribute, value| {
            if element == "img" && attribute == "src" {
                return cid_srcs
                    .contains(value)
                    .then(|| Cow::Owned(value.to_string()));
            }
            if attribute == "href" && value.trim().to_ascii_lowercase().starts_with("cid:") {
                return None;
            }
            if attribute == "style" {
                return known_styles
                    .contains(value)
                    .then(|| Cow::Owned(value.to_string()));
            }
            Some(Cow::Owned(value.to_string()))
        })
        .clean(themed)
        .to_string();
    cleaned == themed
}

fn render_plain(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = highlight_fences(signature_hard_breaks(Parser::new_ext(markdown, options)));
    let mut unsafe_html = String::with_capacity(markdown.len() * 2);
    html::push_html(&mut unsafe_html, parser.into_iter());
    sanitize_compose_html(&unsafe_html)
}

/// [`render_plain`] with embedded images admitted (`cids` is non-empty and
/// every member is a safe Content-ID).
fn render_with_images(markdown: &str, cids: &HashSet<&str>) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = highlight_fences(
        inline_images(
            signature_hard_breaks(Parser::new_ext(markdown, options)),
            cids,
        )
        .into_iter(),
    );
    let mut unsafe_html = String::with_capacity(markdown.len() * 2);
    html::push_html(&mut unsafe_html, parser.into_iter());
    sanitize_compose_html_with_images(&unsafe_html, cids)
}

/// Event adapter: an image whose URL is exactly `cid:<id>` with `<id>` in
/// `cids` becomes one `<img>` event (alt text escaped, fixed style); every
/// other image is consumed and dropped, which is what the sanitiser does to
/// it without this module.
fn inline_images<'a>(
    events: impl Iterator<Item = Event<'a>>,
    cids: &HashSet<&str>,
) -> Vec<Event<'a>> {
    let mut out = Vec::new();
    let mut events = events.peekable();
    while let Some(ev) = events.next() {
        let Event::Start(Tag::Image { dest_url, .. }) = &ev else {
            out.push(ev);
            continue;
        };
        let allowed = dest_url
            .strip_prefix("cid:")
            .filter(|id| cids.contains(id))
            .map(str::to_string);
        let mut alt = String::new();
        let mut depth = 1usize;
        for inner in events.by_ref() {
            match inner {
                Event::Start(Tag::Image { .. }) => depth += 1,
                Event::End(TagEnd::Image) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Event::Text(t) | Event::Code(t) => alt.push_str(&t),
                Event::SoftBreak | Event::HardBreak => alt.push(' '),
                _ => {}
            }
        }
        if let Some(id) = allowed {
            let alt = alt
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;");
            out.push(Event::Html(CowStr::from(format!(
                "<img src=\"cid:{id}\" alt=\"{alt}\" style=\"{INLINE_IMG_STYLE}\">"
            ))));
        }
    }
    out
}

/// Event adapter: inside a paragraph that opens with the `-- ` signature
/// delimiter (a lone `--` Text followed by a SoftBreak, which is what the
/// app-inserted signature block produces) every SoftBreak becomes a HardBreak,
/// so the signature keeps its line structure. All other paragraphs, and every
/// other event, pass through untouched.
fn signature_hard_breaks<'a>(
    events: impl Iterator<Item = Event<'a>>,
) -> impl Iterator<Item = Event<'a>> {
    let mut events = events.peekable();
    let mut in_signature = false;
    let mut para_start = false;
    std::iter::from_fn(move || {
        let ev = events.next()?;
        Some(match ev {
            Event::Start(Tag::Paragraph) => {
                para_start = true;
                in_signature = false;
                ev
            }
            Event::End(TagEnd::Paragraph) => {
                para_start = false;
                in_signature = false;
                ev
            }
            Event::Text(ref t) if para_start => {
                para_start = false;
                in_signature =
                    t.as_ref() == "--" && matches!(events.peek(), Some(Event::SoftBreak));
                ev
            }
            Event::SoftBreak if in_signature => Event::HardBreak,
            other => {
                para_start = false;
                other
            }
        })
    })
}

/// Event adapter: a fenced block whose info-string language the highlighter
/// knows becomes a ready `<pre><code>` with inline-styled token spans (the
/// sanitiser then re-validates every span/style). Blocks with no/unknown
/// language, and indented blocks, pass through untouched. Mermaid fences are
/// deliberately NOT special-cased: they stay a plain code block (the source),
/// which every client can show; see project-docs/markdown-mermaid-highlight-eval.md.
fn highlight_fences<'a>(events: impl Iterator<Item = Event<'a>>) -> Vec<Event<'a>> {
    let mut out = Vec::new();
    let mut events = events.peekable();
    while let Some(ev) = events.next() {
        let lang = match &ev {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => info
                .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
                .next()
                .filter(|l| code_highlight::is_supported(l))
                .map(str::to_string),
            _ => None,
        };
        let Some(lang) = lang else {
            out.push(ev);
            continue;
        };
        let mut code = String::new();
        for inner in events.by_ref() {
            match inner {
                Event::End(TagEnd::CodeBlock) => break,
                Event::Text(t) => code.push_str(&t),
                _ => {}
            }
        }
        match code_highlight::highlight_code(&lang, &code) {
            Some(h) => out.push(Event::Html(CowStr::from(format!(
                "<pre><code>{h}</code></pre>\n"
            )))),
            None => {
                out.push(ev);
                out.push(Event::Text(CowStr::from(code)));
                out.push(Event::End(TagEnd::CodeBlock));
            }
        }
    }
    out
}

/// The compose-specific ammonia policy: a small structural allowlist, no
/// `img`, `href` only on `a` (http/https/mailto — never `javascript:` or
/// `data:`), and `style` only on `span` with one of the four fixed
/// `code_highlight::token_styles()` values (any other value is dropped, so no
/// CSS `url()` smuggling).
/// Reused nowhere else — `sanitize_email_html` (incoming mail,
/// `native/crates/castellum-mail/src/sanitize.rs:96`) has a separate, wider
/// policy on purpose: that one is defending against adversarial SENDERS,
/// this one is defending Castellum's own recipients from whatever the LOCAL
/// user's Markdown happens to render to.
fn sanitize_compose_html(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    let mut tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::new();
    tag_attributes.insert("a", ["href"].into_iter().collect());
    tag_attributes.insert("span", ["style"].into_iter().collect());

    ammonia::Builder::default()
        .tags(ALLOWED_TAGS.iter().copied().collect())
        .generic_attributes(HashSet::new())
        .tag_attributes(tag_attributes)
        .clean_content_tags(["script", "style"].into_iter().collect())
        .url_schemes(["http", "https", "mailto"].into_iter().collect())
        .attribute_filter(|_element, attribute, value| {
            if attribute == "style" {
                // Only the highlighter's own fixed colour values; anything a
                // user smuggles in (url(), position, ...) is dropped.
                return code_highlight::token_styles()
                    .contains(&value)
                    .then_some(Cow::Borrowed(value));
            }
            if attribute == "href" {
                let v = value.trim().to_ascii_lowercase();
                if v.starts_with("javascript:") || v.starts_with("data:") {
                    return None;
                }
            }
            Some(Cow::Borrowed(value))
        })
        .clean(html)
        .to_string()
}

/// [`sanitize_compose_html`] plus `<img src="cid:ID" alt style>` for the IDs in
/// `cids`. `img src` survives only as `cid:` + a member of the set, `img style`
/// only as [`INLINE_IMG_STYLE`], and no `href` may use the `cid:` scheme.
fn sanitize_compose_html_with_images(html: &str, cids: &HashSet<&str>) -> String {
    if html.is_empty() {
        return String::new();
    }
    let mut tags: HashSet<&str> = ALLOWED_TAGS.iter().copied().collect();
    tags.insert("img");
    let mut tag_attributes: HashMap<&str, HashSet<&str>> = HashMap::new();
    tag_attributes.insert("a", ["href"].into_iter().collect());
    tag_attributes.insert("span", ["style"].into_iter().collect());
    tag_attributes.insert("img", ["src", "alt", "style"].into_iter().collect());
    let cid_srcs: HashSet<String> = cids.iter().map(|c| format!("cid:{c}")).collect();

    ammonia::Builder::default()
        .tags(tags)
        .generic_attributes(HashSet::new())
        .tag_attributes(tag_attributes)
        .clean_content_tags(["script", "style"].into_iter().collect())
        .url_schemes(["http", "https", "mailto", "cid"].into_iter().collect())
        .attribute_filter(move |element, attribute, value| {
            if element == "img" {
                return match attribute {
                    "src" => cid_srcs
                        .contains(value)
                        .then(|| Cow::Owned(value.to_string())),
                    "style" => (value == INLINE_IMG_STYLE).then(|| Cow::Owned(value.to_string())),
                    _ => Some(Cow::Owned(value.to_string())),
                };
            }
            if attribute == "style" {
                return code_highlight::token_styles()
                    .contains(&value)
                    .then(|| Cow::Owned(value.to_string()));
            }
            if attribute == "href" {
                let v = value.trim().to_ascii_lowercase();
                if v.starts_with("javascript:") || v.starts_with("data:") || v.starts_with("cid:") {
                    return None;
                }
            }
            Some(Cow::Owned(value.to_string()))
        })
        .clean(html)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_common_mark_basics() {
        let html = render_markdown_to_html("# Title\n\n**bold** and *em* and `code`.\n");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>em</em>"));
        assert!(html.contains("<code>code</code>"));
    }

    #[test]
    fn renders_lists_and_blockquotes() {
        let html = render_markdown_to_html("- one\n- two\n\n> quoted line\n");
        assert!(html.contains("<ul>"));
        assert!(html.contains("<li>one</li>"));
        assert!(html.contains("<blockquote>"));
    }

    #[test]
    fn renders_gfm_tables_and_strikethrough() {
        let html = render_markdown_to_html("~~gone~~\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<del>gone</del>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("<td>1</td>"));
    }

    #[test]
    fn links_survive_http_and_mailto_but_not_javascript_or_data() {
        let html = render_markdown_to_html(
            "[safe](https://example.com) [mail](mailto:a@b.com) [bad](javascript:alert(1))",
        );
        assert!(html.contains(r#"href="https://example.com""#));
        assert!(html.contains(r#"href="mailto:a@b.com""#));
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn never_emits_an_img_tag_even_for_markdown_image_syntax() {
        let html = render_markdown_to_html("![alt text](https://evil.example/track.png)");
        assert!(!html.contains("<img"));
        // ammonia unwraps a disallowed tag to its text content (the alt
        // text), but the remote URL — the attribute value — is dropped.
        assert!(!html.contains("evil.example"));
    }

    #[test]
    fn strips_script_and_inline_event_handlers_entirely() {
        let html = render_markdown_to_html(
            "before\n\n<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\nafter",
        );
        let lower = html.to_ascii_lowercase();
        assert!(!lower.contains("<script"));
        assert!(!lower.contains("onerror"));
        assert!(html.contains("before"));
        assert!(html.contains("after"));
    }

    #[test]
    fn empty_and_whitespace_only_input_renders_to_empty_or_near_empty() {
        assert_eq!(render_markdown_to_html(""), "");
        let html = render_markdown_to_html("   \n\n  \n");
        assert!(!html.contains("<h1"), "no stray heading from blank input");
    }

    #[test]
    fn malformed_and_adversarial_input_never_panics() {
        let long_run = "*".repeat(50_000);
        let inputs: &[&str] = &[
            "```unterminated fence",
            "[link](",
            "***",
            "# \0\0\0",
            "<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<",
            &long_run,
            "\u{202e}reversed\u{202c}",
        ];
        for input in inputs {
            let _ = render_markdown_to_html(input); // must not panic
        }
    }

    #[test]
    fn rendering_is_deterministic_for_the_same_input() {
        let md = "# Same\n\nEvery time.\n";
        assert_eq!(render_markdown_to_html(md), render_markdown_to_html(md));
    }

    // ---- MM-5 themes ----

    #[test]
    fn plain_theme_is_byte_identical_to_the_s46_renderer() {
        for md in [
            "**bold** and `code`",
            "# Title\n\n> quote\n\n| a | b |\n|---|---|\n| 1 | 2 |",
            "",
            "---",
        ] {
            assert_eq!(
                render_markdown_to_html_themed(md, MarkdownTheme::Plain),
                render_markdown_to_html(md)
            );
        }
    }

    #[test]
    fn themes_add_only_fixed_inline_styles_and_a_wrapper() {
        let md = "# Hi\n\nSome `code` and [a link](https://example.com).\n\n> quoted\n\n```\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        for theme in [
            MarkdownTheme::Clean,
            MarkdownTheme::Serif,
            MarkdownTheme::Compact,
        ] {
            let html = render_markdown_to_html_themed(md, theme);
            assert!(
                html.starts_with("<div style=\"font-family:") && html.ends_with("</div>"),
                "{theme:?}"
            );
            for needle in [
                "<h1 style=",
                "<blockquote style=",
                "<table style=",
                "<th style=",
                "<td style=",
                "<pre style=",
                "<a style=\"color:#0a66c2\" href=\"https://example.com\"",
            ] {
                assert!(html.contains(needle), "{theme:?} missing {needle}: {html}");
            }
            assert!(
                html.contains("color:#1d1d1f;background:#ffffff"),
                "{theme:?} wrapper pairs dark text with a white background"
            );
            assert!(
                html.contains("<code style=\"font-family:Menlo"),
                "inline code styled"
            );
            assert!(
                html.contains("background:none;padding:0"),
                "code inside pre is reset"
            );
            assert!(
                !html.contains("<script") && !html.contains("<img") && !html.contains("<style")
            );
        }
    }

    #[test]
    fn user_text_can_never_reach_a_style_or_add_an_attribute() {
        // The sanitiser runs first; a hostile source cannot smuggle style/onclick/img/javascript: links.
        let hostile = "<p style=\"x\" onclick=\"evil()\">hi</p> <img src=x onerror=alert(1)> [x](javascript:alert(1)) <a href=\"data:text/html,x\">d</a> <style>p{background:url(http://t)}</style>";
        for theme in MarkdownTheme::ALL {
            let html = render_markdown_to_html_themed(hostile, theme);
            for bad in [
                "onclick",
                "onerror",
                "<img",
                // The literal text "javascript:alert(1)" may remain as inert
                // text; a live href / style / element must never appear.
                "href=\"javascript",
                "href=\"data",
                "url(",
                "<style",
                "evil()",
            ] {
                assert!(!html.contains(bad), "{theme:?} leaked {bad}: {html}");
            }
        }
    }

    #[test]
    fn theme_names_round_trip_and_unknown_is_plain() {
        for t in MarkdownTheme::ALL {
            assert_eq!(MarkdownTheme::from_name(t.name()), t);
        }
        assert_eq!(MarkdownTheme::from_name(" SERIF "), MarkdownTheme::Serif);
        assert_eq!(MarkdownTheme::from_name("neon"), MarkdownTheme::Plain);
        assert_eq!(MarkdownTheme::from_name(""), MarkdownTheme::Plain);
    }

    #[test]
    fn themed_rendering_is_deterministic_and_handles_empty_input() {
        assert_eq!(render_markdown_to_html_themed("", MarkdownTheme::Clean), "");
        let a = render_markdown_to_html_themed("*x*", MarkdownTheme::Serif);
        assert_eq!(
            a,
            render_markdown_to_html_themed("*x*", MarkdownTheme::Serif)
        );
    }

    /// MM-5 review: raw HTML in Markdown with a tag INSIDE an href value. It is
    /// inert today (the serialiser escapes `<` in attributes); this pins it for
    /// every theme so a dependency change cannot silently reopen an injection.
    #[test]
    fn a_tag_inside_an_href_never_becomes_live_markup() {
        let hostile = "<a href=\"https://e.com/<p><img src=x onerror=alert(1)>\">link</a>\n\n[x](https://e.com/\"><img src=y onerror=z>)";
        for theme in MarkdownTheme::ALL {
            let html = render_markdown_to_html_themed(hostile, theme);
            assert!(!html.contains("<img"), "{theme:?}: {html}");
            assert!(
                !html.contains("onerror=") || !html.contains("<img"),
                "{theme:?}: {html}"
            );
            assert!(html.matches("href=").count() <= 2, "{theme:?}: {html}");
        }
    }

    /// The fail-closed check itself: if theming ever produced live markup the
    /// result would differ from a strict re-sanitise, and the plain HTML wins.
    #[test]
    fn verification_rejects_injected_markup_and_accepts_real_theme_output() {
        for theme in [
            MarkdownTheme::Clean,
            MarkdownTheme::Serif,
            MarkdownTheme::Compact,
        ] {
            let html = render_markdown_to_html_themed(
                "# T\n\n`c` [a](https://x.test)\n\n```\nb\n```\n\n> q",
                theme,
            );
            let (wrapper, tags) = theme.styles();
            assert!(
                verified_theme_output(&html, &wrapper, &tags, &HashSet::new()),
                "real output must verify: {html}"
            );
            let poisoned = html.replace("<p style=", "<img src=x onerror=alert(1)><p style=");
            assert!(!verified_theme_output(
                &poisoned,
                &wrapper,
                &tags,
                &HashSet::new()
            ));
            let restyled = html.replace("color:#0a66c2", "background:url(http://t/)");
            assert!(
                !verified_theme_output(&restyled, &wrapper, &tags, &HashSet::new()),
                "unknown style values are rejected"
            );
        }
    }

    #[test]
    fn signature_block_keeps_its_line_breaks_in_every_theme() {
        let md = "hi\n\n-- \nAlice Puig\nCastellum tester\n+34 600";
        for theme in MarkdownTheme::ALL {
            let html = render_markdown_to_html_themed(md, theme);
            assert!(html.contains("Alice Puig<br"), "{theme:?}: {html}");
            assert!(html.contains("Castellum tester<br"), "{theme:?}: {html}");
        }
    }

    #[test]
    fn ordinary_two_line_paragraph_still_has_no_br() {
        let html = render_markdown_to_html("one\ntwo\n\n-- not a signature start\nx\ny");
        assert!(!html.contains("<br"), "{html}");
    }

    // ---- #3148 fenced-code highlighting ----

    #[test]
    fn tagged_fence_is_highlighted_with_inline_styles_in_every_theme() {
        let md = "```rust\nlet x = 1; // n\n```\n";
        for theme in MarkdownTheme::ALL {
            let html = render_markdown_to_html_themed(md, theme);
            assert!(
                html.contains("<span style=\"color:#c2338a\">let</span>"),
                "{theme:?}: {html}"
            );
            assert!(html.contains("color:#7a828c\">// n</span>"), "{theme:?}");
            assert!(!html.contains("class="), "no JS/CSS-class dependency");
            assert!(!html.contains("<script"));
        }
    }

    #[test]
    fn themed_highlight_keeps_theme_and_survives_verification() {
        let html = render_markdown_to_html_themed("```py\nx = 'a'\n```", MarkdownTheme::Clean);
        assert!(html.starts_with("<div style="), "{html}");
        assert!(html.contains("<pre style="), "{html}");
        assert!(html.contains("color:#2f8f4e"), "{html}");
    }

    #[test]
    fn untagged_unknown_mermaid_and_indented_blocks_stay_plain() {
        for md in [
            "```\nlet x = 1;\n```",
            "```nosuchlang\nlet x = 1;\n```",
            "```mermaid\ngraph TD; A-->B;\n```",
            "    let x = 1;\n",
        ] {
            let html = render_markdown_to_html(md);
            assert!(!html.contains("<span"), "{md}: {html}");
            assert!(html.contains("<pre><code>"), "{md}: {html}");
        }
        assert!(render_markdown_to_html("```mermaid\nA-->B\n```").contains("A--&gt;B"));
    }

    #[test]
    fn info_strings_with_attributes_still_pick_the_language() {
        for info in ["rust,ignore", "rust {.numberLines}", "rust{1,3}"] {
            let html = render_markdown_to_html(&format!("```{info}\nlet x = 1;\n```"));
            assert!(html.contains("color:#c2338a"), "{info}: {html}");
        }
    }

    #[test]
    fn hostile_fence_content_and_user_spans_cannot_inject() {
        let html = render_markdown_to_html(
            "```js\n</code></pre><script>alert(1)</script>\n```\n\n<span style=\"position:fixed;background:url(http://evil.example/x)\">t</span>",
        );
        let lower = html.to_ascii_lowercase();
        assert!(!lower.contains("<script"), "{html}");
        assert!(!lower.contains("evil.example"), "{html}");
        assert!(!lower.contains("position:fixed"), "{html}");
    }

    #[test]
    fn highlighted_render_is_deterministic_and_text_preserving() {
        let md = "```sql\nSELECT 'a<b' FROM t; -- c\n```";
        assert_eq!(render_markdown_to_html(md), render_markdown_to_html(md));
        assert!(
            render_markdown_to_html(md).contains("&#39;a&lt;b&#39;")
                || render_markdown_to_html(md).contains("&lt;b")
        );
    }

    // ---- inline images ----

    const CID: &str = "img-0123456789ab@castellum";

    fn with_images(md: &str, theme: MarkdownTheme) -> String {
        render_markdown_to_html_with_images(md, theme, &[CID])
    }

    #[test]
    fn empty_cid_set_is_byte_identical_to_themed_render() {
        let inputs = [
            "# Title\n\n**bold** [link](https://x.example) `code`\n\n- a\n- b",
            "![alt](https://evil.example/t.png) and ![x](cid:img-0123456789ab@castellum)",
            "```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |",
            "-- \nSignature line\nsecond",
            "<script>alert(1)</script><img src=x onerror=alert(1)>",
        ];
        for theme in [
            MarkdownTheme::Plain,
            MarkdownTheme::Clean,
            MarkdownTheme::Serif,
        ] {
            for md in inputs {
                assert_eq!(
                    render_markdown_to_html_with_images(md, theme, &[]),
                    render_markdown_to_html_themed(md, theme),
                    "{theme:?} {md:?}"
                );
                // Unsafe ids are ignored, which is the same as an empty set.
                assert_eq!(
                    render_markdown_to_html_with_images(md, theme, &["bad id", "a\"b", ""]),
                    render_markdown_to_html_themed(md, theme)
                );
            }
        }
    }

    #[test]
    fn allowed_cid_image_renders_img_with_alt_and_fixed_style() {
        let html = with_images(
            &format!("Look:\n\n![My shot](cid:{CID})"),
            MarkdownTheme::Plain,
        );
        assert!(
            html.contains(&format!(
                "<img src=\"cid:{CID}\" alt=\"My shot\" style=\"{INLINE_IMG_STYLE}\">"
            )) || (html.contains(&format!("src=\"cid:{CID}\""))
                && html.contains("alt=\"My shot\"")
                && html.contains(INLINE_IMG_STYLE)),
            "{html}"
        );
        assert_eq!(referenced_cids(&html), vec![CID.to_string()]);
    }

    #[test]
    fn unknown_cid_remote_and_data_images_are_dropped_when_images_are_enabled() {
        let md = format!(
            "![a](cid:img-ffffffffffff@castellum) ![b](https://evil.example/t.png) ![c](data:image/png;base64,AAAA) ![ok](cid:{CID})"
        );
        let html = with_images(&md, MarkdownTheme::Plain);
        assert!(!html.contains("evil.example"), "{html}");
        assert!(!html.contains("data:"), "{html}");
        assert!(!html.contains("ffffffffffff"), "{html}");
        assert_eq!(referenced_cids(&html), vec![CID.to_string()]);
        assert_eq!(html.matches("<img").count(), 1, "{html}");
    }

    #[test]
    fn raw_html_img_keeps_no_src_unless_an_allowed_cid() {
        let md = format!(
            "<img src=\"https://evil.example/t.png\"> <img src=\"cid:{CID}\"> <img src=\"cid:img-ffffffffffff@castellum\" style=\"position:fixed\">"
        );
        let html = with_images(&md, MarkdownTheme::Plain);
        assert!(!html.contains("evil.example"), "{html}");
        assert!(!html.contains("ffffffffffff"), "{html}");
        assert!(!html.contains("position"), "{html}");
        assert_eq!(referenced_cids(&html), vec![CID.to_string()]);
    }

    #[test]
    fn href_with_cid_scheme_is_rejected() {
        let html = with_images(&format!("[click](cid:{CID})"), MarkdownTheme::Plain);
        assert!(!html.contains("cid:"), "{html}");
        let html = with_images(
            &format!("<a href=\"cid:{CID}\">x</a>"),
            MarkdownTheme::Plain,
        );
        assert!(!html.contains("cid:"), "{html}");
    }

    #[test]
    fn alt_text_with_quotes_and_tags_cannot_break_out_of_the_attribute() {
        let html = with_images(
            &format!("![\" onerror=\"alert(1)\" <b>x</b> '](cid:{CID})"),
            MarkdownTheme::Plain,
        );
        let lower = html.to_ascii_lowercase();
        assert!(!lower.contains("onerror=\"alert"), "{html}");
        assert!(!lower.contains("<b>"), "{html}");
        assert_eq!(html.matches("<img").count(), 1, "{html}");
    }

    #[test]
    fn every_theme_survives_verification_with_an_image() {
        for theme in [
            MarkdownTheme::Plain,
            MarkdownTheme::Clean,
            MarkdownTheme::Serif,
        ] {
            let html = with_images(
                &format!("# Hi\n\nText with ![shot](cid:{CID}) inline.\n\n> quote"),
                theme,
            );
            assert!(html.contains("<img"), "{theme:?}: {html}");
            if theme != MarkdownTheme::Plain {
                assert!(
                    html.contains("<div style="),
                    "theme kept, not dropped: {html}"
                );
            }
        }
    }

    #[test]
    fn hostile_cid_strings_are_not_safe_content_ids() {
        for bad in [
            "",
            "a b",
            "a\"b",
            "a<b>",
            "a:b",
            "a/b",
            "a\nb",
            &"x".repeat(129),
        ] {
            assert!(!is_safe_content_id(bad), "{bad:?}");
        }
        assert!(is_safe_content_id(CID));
        assert!(is_safe_content_id(&"x".repeat(128)));
    }

    #[test]
    fn referenced_cids_lists_each_once_in_order() {
        let html = "<img src=\"cid:b@x\"><p>t</p><img src=\"cid:a@x\"><img src=\"cid:b@x\"><img src=\"https://r\"><img src=\"cid:";
        assert_eq!(
            referenced_cids(html),
            vec!["b@x".to_string(), "a@x".to_string()]
        );
        assert!(referenced_cids("<p>none</p>").is_empty());
    }
}
