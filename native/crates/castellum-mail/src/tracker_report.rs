// Castellum - what remote content a message tried to load.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The reader already blocks remote images and shows "Remote images blocked".
// This module says MORE about what was blocked, in the spirit of HEY's spy
// pixel report: how many of the remote references look like tracking (a
// 1x1 / 0x0 pixel, or an image served by a known mail-tracking service) and
// which services those are. It runs over the already-SANITIZED html (ammonia
// normalises attributes to `name="value"`), is pure and total, and never
// fetches anything.

use serde::Serialize;
use std::collections::BTreeMap;

/// Host suffix -> service name. A host matches when it equals the suffix or is
/// a subdomain of it. Deliberately a short, well-known list: the report names
/// services we are confident about; everything else is counted as a remote
/// image (or as a tracker when it is a pixel).
const KNOWN_TRACKERS: &[(&str, &str)] = &[
    ("list-manage.com", "Mailchimp"),
    ("mailchimp.com", "Mailchimp"),
    ("mcusercontent.com", "Mailchimp"),
    ("hubspotemail.net", "HubSpot"),
    ("hs-analytics.net", "HubSpot"),
    ("hsforms.com", "HubSpot"),
    ("sendgrid.net", "SendGrid"),
    ("mailgun.org", "Mailgun"),
    ("mandrillapp.com", "Mandrill"),
    ("mailchimpapp.net", "Mailchimp"),
    ("google-analytics.com", "Google Analytics"),
    ("doubleclick.net", "Google Ads"),
    ("exacttarget.com", "Salesforce Marketing Cloud"),
    ("sfmc-content.com", "Salesforce Marketing Cloud"),
    ("createsend.com", "Campaign Monitor"),
    ("cmail19.com", "Campaign Monitor"),
    ("cmail20.com", "Campaign Monitor"),
    ("klaviyo.com", "Klaviyo"),
    ("klaviyomail.com", "Klaviyo"),
    ("sendinblue.com", "Brevo"),
    ("brevo.com", "Brevo"),
    ("sibautomation.com", "Brevo"),
    ("pstmrk.it", "Postmark"),
    ("postmarkapp.com", "Postmark"),
    ("awstrack.me", "Amazon SES"),
    ("mktoresp.com", "Marketo"),
    ("marketo.com", "Marketo"),
    ("pardot.com", "Pardot"),
    ("activehosted.com", "ActiveCampaign"),
    ("convertkit.com", "ConvertKit"),
    ("customeriomail.com", "Customer.io"),
    ("customer.io", "Customer.io"),
    ("intercom-mail.com", "Intercom"),
    ("litmus.com", "Litmus"),
    ("emltrk.com", "Litmus"),
    ("mixpanel.com", "Mixpanel"),
    ("segment.io", "Segment"),
    ("facebook.com", "Facebook"),
    ("linkedin.com", "LinkedIn"),
    ("t.co", "X"),
    ("bounceexchange.com", "Bounce Exchange"),
    ("rs6.net", "Constant Contact"),
    ("constantcontact.com", "Constant Contact"),
    ("sparkpostmail.com", "SparkPost"),
    ("rsgsv.net", "Mailchimp"),
    ("ci3.googleusercontent.com", "Gmail image proxy"),
];

/// Most HTML bytes analysed. The report runs on a hostile body: past this the
/// rest is ignored and the report says `truncated`.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;
/// Most remote references counted; past this the scan stops (`truncated`).
pub const MAX_REFERENCES: usize = 500;

/// How many remote references a message holds and how many look like tracking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemoteContentReport {
    /// Every remote (`http`, `https`, `//`) image or CSS `url()` reference.
    pub remote_count: usize,
    /// Of those, 1x1 / 0x0 pixels or images from a known tracking service.
    pub tracker_count: usize,
    /// Of the trackers, how many were pixels (size <= 1x1).
    pub pixel_count: usize,
    /// Named tracking services, most references first.
    pub services: Vec<TrackerService>,
    /// Remote hosts by reference count, most first (at most 10).
    pub hosts: Vec<HostCount>,
    /// The input was longer than [`MAX_INPUT_BYTES`] or held more than
    /// [`MAX_REFERENCES`] references; the counts are a lower bound.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrackerService {
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostCount {
    pub host: String,
    pub count: usize,
}

/// The service a host belongs to, if it is a known tracker.
pub fn tracker_service_for_host(host: &str) -> Option<&'static str> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    KNOWN_TRACKERS
        .iter()
        .find(|(suffix, _)| host == *suffix || host.ends_with(&format!(".{suffix}")))
        .map(|(_, name)| *name)
}

/// Analyse sanitized email HTML. Total over any input.
pub fn analyze_remote_content(sanitized_html: &str) -> RemoteContentReport {
    let mut truncated = false;
    let sanitized_html = if sanitized_html.len() > MAX_INPUT_BYTES {
        truncated = true;
        let mut cut = MAX_INPUT_BYTES;
        while !sanitized_html.is_char_boundary(cut) {
            cut -= 1;
        }
        &sanitized_html[..cut]
    } else {
        sanitized_html
    };
    let mut remote_count = 0usize;
    let mut tracker_count = 0usize;
    let mut pixel_count = 0usize;
    let mut services: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut hosts: BTreeMap<String, usize> = BTreeMap::new();

    let seen = std::cell::Cell::new(0usize);
    let mut note = |url: &str, is_pixel: bool| {
        seen.set(seen.get() + 1);
        let Some(host) = remote_host(url) else {
            return;
        };
        remote_count += 1;
        *hosts.entry(host.clone()).or_default() += 1;
        let service = tracker_service_for_host(&host);
        if is_pixel || service.is_some() {
            tracker_count += 1;
            if is_pixel {
                pixel_count += 1;
            }
        }
        if let Some(name) = service {
            *services.entry(name).or_default() += 1;
        }
    };

    'scan: for tag in tags(sanitized_html) {
        if seen.get() >= MAX_REFERENCES {
            truncated = true;
            break 'scan;
        }
        let attrs = attributes(tag);
        let is_img = tag.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("<img"));
        if is_img {
            if let Some(src) = attrs.get("src") {
                note(src, is_pixel(&attrs));
            }
        }
        if let Some(style) = attrs.get("style") {
            for url in css_urls(style, MAX_REFERENCES) {
                if seen.get() >= MAX_REFERENCES {
                    truncated = true;
                    break 'scan;
                }
                note(&url, false);
            }
        }
    }

    let mut services: Vec<TrackerService> = services
        .into_iter()
        .map(|(name, count)| TrackerService {
            name: name.to_string(),
            count,
        })
        .collect();
    services.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    let mut hosts: Vec<HostCount> = hosts
        .into_iter()
        .map(|(host, count)| HostCount { host, count })
        .collect();
    hosts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.host.cmp(&b.host)));
    hosts.truncate(10);

    RemoteContentReport {
        remote_count,
        tracker_count,
        pixel_count,
        services,
        hosts,
        truncated,
    }
}

/// Each `<...>` tag's text, in order. A stray `<` with no `>` ends the scan.
/// A `>` inside a double-quoted attribute value does not end the tag: the
/// sanitizer leaves `>` unescaped there, so `<img alt="x>" src=...>` would
/// otherwise be cut before its `src` and the pixel never counted.
fn tags(html: &str) -> impl Iterator<Item = &str> {
    let mut rest = html;
    std::iter::from_fn(move || {
        let start = rest.find('<')?;
        let after = &rest[start..];
        let mut quoted = false;
        let end = after.bytes().position(|b| {
            if b == b'"' {
                quoted = !quoted;
            }
            b == b'>' && !quoted
        })?;
        let tag = &after[..=end];
        rest = &after[end + 1..];
        Some(tag)
    })
}

/// `name="value"` pairs of one tag (ammonia emits double quotes). Names are
/// lower-cased; values keep their text with the common entities (`&amp;`, `&quot;`, `&#39;`) decoded.
fn attributes(tag: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = tag;
    while let Some(eq) = rest.find("=\"") {
        let name_start = rest[..eq]
            .rfind(|c: char| c.is_whitespace() || c == '<')
            .map_or(0, |i| i + 1);
        let name = rest[name_start..eq].to_ascii_lowercase();
        let value_start = eq + 2;
        let Some(len) = rest[value_start..].find('"') else {
            break;
        };
        let value = rest[value_start..value_start + len]
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&#x27;", "'")
            .replace("&amp;", "&");
        if !name.is_empty() {
            out.entry(name).or_insert(value);
        }
        rest = &rest[value_start + len + 1..];
    }
    out
}

/// A tracking pixel: width and/or height of 0 or 1 (attribute or inline CSS).
fn is_pixel(attrs: &BTreeMap<String, String>) -> bool {
    let tiny = |v: &str| {
        let v = v.trim().trim_end_matches("px").trim();
        matches!(v, "0" | "1")
    };
    let css = |prop: &str| -> Option<String> {
        let style = attrs.get("style")?.to_ascii_lowercase();
        style.split(';').find_map(|decl| {
            let (k, v) = decl.split_once(':')?;
            (k.trim() == prop).then(|| v.trim().to_string())
        })
    };
    let w = attrs.get("width").cloned().or_else(|| css("width"));
    let h = attrs.get("height").cloned().or_else(|| css("height"));
    match (w, h) {
        (Some(w), Some(h)) => tiny(&w) && tiny(&h),
        (Some(w), None) => tiny(&w),
        (None, Some(h)) => tiny(&h),
        (None, None) => false,
    }
}

/// Every `url(...)` target in a CSS declaration block, at most `limit`.
/// One linear pass: the cursor only moves forward, so a hostile block of
/// 100k `url(` with no `)` costs one failed search, not 100k.
fn css_urls(style: &str, limit: usize) -> Vec<String> {
    // ASCII lower-casing keeps byte offsets identical to `style`.
    let lower = style.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while out.len() < limit {
        let Some(found) = lower[pos..].find("url(") else {
            break;
        };
        let start = pos + found + 4;
        let Some(len) = style[start..].find(')') else {
            break; // no closing paren anywhere after: nothing more to find
        };
        let url = style[start..start + len]
            .trim()
            .trim_matches(|c| c == '\'' || c == '"')
            .trim_start_matches("&quot;")
            .trim_end_matches("&quot;")
            .trim();
        out.push(url.to_string());
        pos = start + len + 1;
    }
    out
}

/// The lower-cased host of a remote (`http`, `https`, `//`) URL; `None` for
/// `cid:`, `data:`, relative or unparseable references.
fn remote_host(url: &str) -> Option<String> {
    let url = url.trim();
    let full = if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_string()
    };
    let parsed = url::Url::parse(&full).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.host_str().map(|h| h.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::sanitize_email_html;

    fn report(html: &str) -> RemoteContentReport {
        analyze_remote_content(&sanitize_email_html(html).html)
    }

    #[test]
    fn a_message_without_remote_content_reports_nothing() {
        let r = report(
            r#"<p>hi</p><img src="cid:logo"><img src="data:image/png;base64,iVBORw0KGgo=">"#,
        );
        assert_eq!(r, RemoteContentReport::default());
    }

    #[test]
    fn a_one_pixel_image_is_a_tracker_even_from_an_unknown_host() {
        let r = report(r#"<img src="https://unknown.example/p.gif" width="1" height="1">"#);
        assert_eq!((r.remote_count, r.tracker_count, r.pixel_count), (1, 1, 1));
        assert!(r.services.is_empty());
        assert_eq!(r.hosts[0].host, "unknown.example");
    }

    #[test]
    fn a_quoted_gt_in_an_attribute_does_not_hide_the_pixel() {
        let r =
            report(r#"<img alt="x>" src="https://unknown.example/p.gif" width="1" height="1">"#);
        assert_eq!((r.remote_count, r.tracker_count, r.pixel_count), (1, 1, 1));
    }

    #[test]
    fn inline_css_zero_size_counts_as_a_pixel() {
        let r = report(r#"<img src="https://t.example/a" style="width:0px;height:0px">"#);
        assert_eq!(r.pixel_count, 1);
    }

    #[test]
    fn a_known_service_is_named_and_counted() {
        let r = report(
            r#"<img src="https://mailchi.list-manage.com/track/open.php?u=1&amp;id=2" width="1" height="1">
               <img src="https://image.list-manage.com/banner.png" width="600" height="200">
               <img src="https://sendgrid.net/wf/open?upn=x" width="1" height="1">"#,
        );
        assert_eq!(r.remote_count, 3);
        assert_eq!(
            r.tracker_count, 3,
            "banner from a tracking service counts too"
        );
        assert_eq!(r.pixel_count, 2);
        assert_eq!(
            r.services,
            vec![
                TrackerService {
                    name: "Mailchimp".into(),
                    count: 2
                },
                TrackerService {
                    name: "SendGrid".into(),
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn an_ordinary_remote_image_is_remote_but_not_a_tracker() {
        let r = report(r#"<img src="https://cdn.shop.example/hero.jpg" width="600" height="300">"#);
        assert_eq!((r.remote_count, r.tracker_count), (1, 0));
    }

    #[test]
    fn css_background_urls_are_counted_as_remote() {
        let r = report(
            r#"<div style="background-image:url('https://t.hubspotemail.net/x.png')">x</div>"#,
        );
        assert_eq!(r.remote_count, 1);
        assert_eq!(r.services[0].name, "HubSpot");
    }

    #[test]
    fn protocol_relative_urls_are_remote_and_lookalike_hosts_do_not_match() {
        let r = report(
            r#"<img src="//cdn.example/a.png" width="2" height="2"><img src="https://notlist-manage.com/x" width="9" height="9">"#,
        );
        assert_eq!(r.remote_count, 2);
        assert_eq!(
            r.tracker_count, 0,
            "notlist-manage.com is not list-manage.com"
        );
    }

    #[test]
    fn hosts_are_ranked_and_capped() {
        let imgs: String = (0..15)
            .map(|i| format!(r#"<img src="https://h{i}.example/p" width="5" height="5">"#))
            .collect();
        let r = report(&imgs);
        assert_eq!(r.remote_count, 15);
        assert_eq!(r.hosts.len(), 10);
    }

    #[test]
    fn never_panics_on_hostile_input() {
        for s in [
            "",
            "<",
            "<img",
            "<img src=\"",
            "<img src=\"https://",
            "url(",
            "<a style=\"url(\">",
            "\u{0}<img src=\"//\">",
        ] {
            let _ = analyze_remote_content(s);
        }
        let big = "<img src=\"https://a.example/p\" width=\"1\" height=\"1\">".repeat(20_000);
        assert_eq!(analyze_remote_content(&big).remote_count, MAX_REFERENCES);
    }

    #[test]
    fn pathological_one_megabyte_input_is_capped_and_fast() {
        // 1 MB of `url(` with no `)`, as a style value, and as many tiny images.
        let hostile_style = format!(r#"<div style="{}">x</div>"#, "url(".repeat(250_000));
        let t = std::time::Instant::now();
        let r = analyze_remote_content(&hostile_style);
        assert!(t.elapsed().as_millis() < 100, "took {:?}", t.elapsed());
        assert!(r.truncated, "over the input cap");
        assert_eq!(r.remote_count, 0);

        let many = r#"<img src="https://a.example/p" width="1" height="1">"#.repeat(20_000);
        assert!(many.len() > 1_000_000);
        let t = std::time::Instant::now();
        let r = analyze_remote_content(&many);
        assert!(t.elapsed().as_millis() < 100, "took {:?}", t.elapsed());
        assert!(r.truncated);
        assert_eq!(
            r.remote_count, MAX_REFERENCES,
            "capped at the reference limit"
        );
    }

    #[test]
    fn unclosed_url_does_not_hide_earlier_references() {
        let r = report(
            r#"<div style="background:url(https://t.hubspotemail.net/x.png);x:url(">y</div>"#,
        );
        assert_eq!(r.remote_count, 1);
        assert!(!r.truncated);
    }

    #[test]
    fn host_matching_is_case_and_dot_insensitive() {
        assert_eq!(
            tracker_service_for_host("Sub.LIST-MANAGE.com."),
            Some("Mailchimp")
        );
        assert_eq!(tracker_service_for_host("example.org"), None);
    }
}
