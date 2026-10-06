//! Fetching external preview images through Slack's public image proxy, as
//! Slack's web client does, so the linked site never sees this client.

use url::{Host, Url};

const PROXY_BASE: &str = "https://slack-imgs.com/?c=1&o1=ro&url=";
const MAX_ORIGINAL_URL_BYTES: usize = 2048;

/// The URL the network fetch uses for an image, or `None` when it must not
/// be fetched at all. Allowlisted (Slack, avatar, GIF service) URLs are
/// fetched as-is; other public http(s) URLs go through the proxy. This is
/// the single decision shared by the request side and the renderer, so
/// they cannot disagree about what will arrive.
pub(crate) fn preview_fetch_url(original: &str) -> Option<String> {
    if crate::slack::supports_native_preview_asset_url(original) {
        Some(original.to_string())
    } else {
        slack_image_proxy_url(original)
    }
}

/// `https://slack-imgs.com/?c=1&o1=ro&url=<encoded original>` for a public
/// http(s) URL that is not already directly fetchable.
pub(crate) fn slack_image_proxy_url(original: &str) -> Option<String> {
    if original.len() > MAX_ORIGINAL_URL_BYTES
        || crate::slack::supports_native_preview_asset_url(original)
    {
        return None;
    }
    let mut url = Url::parse(original).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || !is_public_domain(&url)
    {
        return None;
    }
    url.set_fragment(None);
    Some(format!("{PROXY_BASE}{}", urlencoding::encode(url.as_str())))
}

fn is_public_domain(url: &Url) -> bool {
    let Some(Host::Domain(host)) = url.host() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host.contains('.')
        && !host.starts_with('.')
        && !["localhost", "local", "internal", "lan", "home", "arpa"]
            .iter()
            .any(|reserved| host == *reserved || host.ends_with(&format!(".{reserved}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxied(encoded: &str) -> String {
        format!("{PROXY_BASE}{encoded}")
    }

    #[test]
    fn external_url_is_percent_encoded_into_the_proxy_query() {
        assert_eq!(
            slack_image_proxy_url("https://destroy.spritefusion.com/og.png").as_deref(),
            Some(proxied("https%3A%2F%2Fdestroy.spritefusion.com%2Fog.png").as_str())
        );
    }

    #[test]
    fn query_strings_unicode_and_encoded_chars_survive_one_round_trip() {
        for original in [
            "https://example.com/a.png?w=1&h=2&x=a b",
            "https://example.com/caf\u{e9}/\u{1f600}.png#frag",
            "https://example.com/a%20b.png?q=%26",
            "http://example.com/a.png",
        ] {
            let proxy = slack_image_proxy_url(original).expect(original);
            let parsed = Url::parse(&proxy).unwrap();
            assert_eq!(parsed.host_str(), Some("slack-imgs.com"));
            let pairs: Vec<_> = parsed.query_pairs().collect();
            assert_eq!(pairs.len(), 3, "{proxy}");
            let mut expected = Url::parse(original).unwrap();
            expected.set_fragment(None);
            assert_eq!(pairs[2].1, expected.as_str(), "{original}");
        }
        assert!(slack_image_proxy_url("https://example.com/a%20b.png?q=%26")
            .unwrap()
            .ends_with("a%2520b.png%3Fq%3D%2526"));
    }

    #[test]
    fn unsafe_or_unsupported_urls_are_not_proxied() {
        let too_long = format!("https://example.com/{}", "x".repeat(MAX_ORIGINAL_URL_BYTES));
        for url in [
            "http://localhost/a.png",
            "https://localhost./a.png",
            "https://printer.local/a.png",
            "https://intranet/a.png",
            "http://10.0.0.1/a.png",
            "http://192.168.1.5/a.png",
            "http://127.0.0.1/a.png",
            "http://169.254.169.254/latest",
            "http://[::1]/a.png",
            "https://[2001:db8::1]/a.png",
            "https://user:pw@example.com/a.png",
            "https://user@example.com/a.png",
            "ftp://example.com/a.png",
            "file:///etc/passwd",
            "data:image/png;base64,AAAA",
            "javascript:alert(1)",
            "not a url",
            "",
            too_long.as_str(),
        ] {
            assert_eq!(slack_image_proxy_url(url), None, "{url}");
            assert_eq!(preview_fetch_url(url), None, "{url}");
        }
    }

    #[test]
    fn the_proxy_itself_is_never_proxied() {
        let url = "https://slack-imgs.com/?url=https%3A%2F%2Fexample.com";
        assert_eq!(slack_image_proxy_url(url), None);
        assert_eq!(preview_fetch_url(url).as_deref(), Some(url));
    }

    #[test]
    fn resolver_keeps_allowlisted_urls_direct_and_proxies_the_rest() {
        for direct in [
            "https://files.slack.com/files-pri/T1-F1/a.png",
            "https://avatars.slack-edge.com/a.png",
            "https://media1.giphy.com/a.gif",
            "https://slack-imgs.com/?c=1&o1=ro&url=x",
        ] {
            assert_eq!(preview_fetch_url(direct).as_deref(), Some(direct));
        }
        let external = "https://example.test/og.png";
        assert_eq!(preview_fetch_url(external), slack_image_proxy_url(external));
        assert!(preview_fetch_url(external).unwrap().starts_with(PROXY_BASE));
    }
}
