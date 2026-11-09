// Castellum - Markdown compose preview (S46).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A single stateless entry point so the compose window's live preview and
// the text/html MIME part actually sent (fed via `Draft::body_html`, built
// by `castellum_provider_smtp::build_rfc822`) are ALWAYS the same render —
// see `castellum_mail::render_markdown_to_html`'s doc comment.

/// Render `markdown` to sanitized HTML for the compose preview pane. Pure
/// and total: never panics, touches no database (no `Profile`/handle needed
/// — see `castellum_render_markdown_preview` in the FFI, which mirrors
/// `castellum_version`'s no-handle shape).
pub fn render_markdown_preview(markdown: &str) -> String {
    castellum_mail::render_markdown_to_html(markdown)
}

/// As `render_markdown_preview`, in one of the MM-5 themes (`"plain"`,
/// `"clean"`, `"serif"`, `"compact"`; an unknown name is `plain`). The
/// compose preview and the sent `text/html` part both come from here, so they
/// cannot disagree.
pub fn render_markdown_themed(markdown: &str, theme: &str) -> String {
    castellum_mail::render_markdown_to_html_themed(
        markdown,
        castellum_mail::MarkdownTheme::from_name(theme),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_the_mail_crate_renderer_verbatim() {
        assert_eq!(
            render_markdown_preview("**bold**"),
            castellum_mail::render_markdown_to_html("**bold**")
        );
    }

    #[test]
    fn themed_rendering_matches_the_mail_crate_and_plain_matches_the_preview() {
        assert_eq!(
            render_markdown_themed("**x**", "plain"),
            render_markdown_preview("**x**")
        );
        assert_eq!(
            render_markdown_themed("**x**", "nonsense"),
            render_markdown_preview("**x**")
        );
        assert!(render_markdown_themed("**x**", "clean").contains("<strong>x</strong>"));
        assert!(render_markdown_themed("**x**", "clean").starts_with("<div style="));
    }
}
