use std::sync::OnceLock;

use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder, Url};

const MAX_REDIRECT_HOPS: usize = 10;

pub(crate) fn ensure_tls_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let installation = rustls::crypto::aws_lc_rs::default_provider().install_default();
        debug_assert!(
            installation.is_ok() || rustls::crypto::CryptoProvider::get_default().is_some()
        );
    }
}

/// Host suffixes reserved for private, local or special-use networks. A host
/// equal to, or ending in `.` plus, any of these is never treated as public.
const NON_PUBLIC_HOST_SUFFIXES: &[&str] = &[
    "localhost",
    "localdomain",
    "local",
    "internal",
    "lan",
    "home",
    "arpa",
];

/// Whether `url` names a public DNS domain: not an IP literal, not a
/// single-label intranet name, and not under a private/special-use suffix.
/// The single shared check for outbound fetch and redirect safety.
pub(crate) fn is_public_domain(url: &Url) -> bool {
    let Some(url::Host::Domain(host)) = url.host() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host.starts_with('.')
        || !host.contains('.')
        || host.parse::<std::net::IpAddr>().is_ok()
    {
        return false;
    }
    !NON_PUBLIC_HOST_SUFFIXES.iter().any(|reserved| {
        host == *reserved
            || host
                .strip_suffix(reserved)
                .is_some_and(|rest| rest.ends_with('.'))
    })
}

fn is_loopback_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str().map(|h| h.trim_end_matches('.')),
            Some("localhost" | "127.0.0.1")
        )
}

fn is_allowed_redirect_target(target: &Url, initial: Option<&Url>) -> bool {
    if !target.username().is_empty() || target.password().is_some() {
        return false;
    }
    if initial.is_some_and(is_loopback_url) && is_loopback_url(target) {
        return true;
    }
    target.scheme() == "https" && is_public_domain(target)
}

pub(crate) fn redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECT_HOPS {
            return attempt.error("too many redirects");
        }
        if is_allowed_redirect_target(attempt.url(), attempt.previous().first()) {
            attempt.follow()
        } else {
            attempt.error("redirect to untrusted URL blocked")
        }
    })
}

/// Same scheme, host and port as the first request, with no userinfo.
fn is_same_origin_redirect(target: &Url, initial: Option<&Url>) -> bool {
    let Some(initial) = initial else {
        return false;
    };
    target.username().is_empty()
        && target.password().is_none()
        && target.scheme() == initial.scheme()
        && target.host_str() == initial.host_str()
        && target.port_or_known_default() == initial.port_or_known_default()
}

/// Redirect policy for requests that carry credentials in the body (e.g. a
/// `token=` form field). reqwest strips auth headers on cross-origin hops but
/// replays the body on 307/308, so only same-origin hops are followed.
pub(crate) fn same_origin_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECT_HOPS {
            return attempt.error("too many redirects");
        }
        if is_same_origin_redirect(attempt.url(), attempt.previous().first()) {
            attempt.follow()
        } else {
            attempt.error("cross-origin redirect blocked for credentialed request")
        }
    })
}

pub(crate) fn builder() -> ClientBuilder {
    ensure_tls_crypto_provider();
    reqwest::Client::builder().redirect(redirect_policy())
}

/// Builder for clients that send Slack tokens or OAuth secrets in the
/// request body. Never follows a redirect to another origin.
pub(crate) fn api_builder() -> ClientBuilder {
    ensure_tls_crypto_provider();
    reqwest::Client::builder().redirect(same_origin_redirect_policy())
}

pub(crate) fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("default HTTP client should build")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_setup_is_idempotent_and_builds_clients() {
        ensure_tls_crypto_provider();
        ensure_tls_crypto_provider();

        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        assert!(builder().build().is_ok());
        assert!(api_builder().build().is_ok());
    }

    #[test]
    fn public_domain_check_rejects_private_and_special_use_hosts() {
        for public in [
            "https://slack.com/api",
            "https://files.slack.com/a.png",
            "https://example.test/og.png",
            "https://homedepot.com/a.png",
            "https://my-lan.example.com/a.png",
            "https://localhost.example.com/a.png",
        ] {
            assert!(is_public_domain(&Url::parse(public).unwrap()), "{public}");
        }
        for private in [
            "https://localhost/a",
            "https://localhost./a",
            "https://app.localhost/a",
            "https://printer.local/a",
            "https://nas.localdomain/a",
            "https://service.internal/a",
            "https://router.lan/a",
            "https://lan/a",
            "https://nas.home/a",
            "https://nas.home./a",
            "https://gw.home.arpa/a",
            "https://1.0.0.127.in-addr.arpa/a",
            "https://intranet/a",
            "https://127.0.0.1/a",
            "https://10.0.0.1/a",
            "https://192.168.1.5/a",
            "https://169.254.169.254/latest",
            "https://[::1]/a",
            "https://[fe80::1]/a",
            "foo://10.0.0.1/a",
            "file:///etc/passwd",
        ] {
            assert!(
                !is_public_domain(&Url::parse(private).unwrap()),
                "{private}"
            );
        }
    }

    #[test]
    fn credentialed_redirect_policy_only_follows_same_origin() {
        let api = Url::parse("https://slack.com/api/chat.postMessage").unwrap();
        let same_origin = Url::parse("https://slack.com/api/chat.postMessage?retry=1").unwrap();
        let explicit_port = Url::parse("https://slack.com:443/api/other").unwrap();
        let other_slack_host = Url::parse("https://files.slack.com/api/x").unwrap();
        let attacker = Url::parse("https://attacker.example/collect").unwrap();
        let downgrade = Url::parse("http://slack.com/api/x").unwrap();
        let other_port = Url::parse("https://slack.com:8443/api/x").unwrap();
        let credentialed = Url::parse("https://user:pw@slack.com/api/x").unwrap();

        assert!(is_same_origin_redirect(&same_origin, Some(&api)));
        assert!(is_same_origin_redirect(&explicit_port, Some(&api)));
        for blocked in [
            &other_slack_host,
            &attacker,
            &downgrade,
            &other_port,
            &credentialed,
        ] {
            assert!(!is_same_origin_redirect(blocked, Some(&api)), "{blocked}");
        }
        assert!(!is_same_origin_redirect(&same_origin, None));
    }

    #[test]
    fn api_client_does_not_replay_body_to_cross_origin_redirect() {
        use tiny_http::{Header, Response, Server};

        let server = Server::http("127.0.0.1:0").expect("mock server should start");
        let address = server.server_addr();
        let handled = std::thread::spawn(move || {
            let request = server.recv().expect("first request should arrive");
            request
                .respond(
                    Response::empty(307).with_header(
                        Header::from_bytes("Location", "https://attacker.example/collect")
                            .expect("location header should be valid"),
                    ),
                )
                .expect("redirect response should be sent");
        });

        let error = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(
                api_builder()
                    .build()
                    .expect("API client should build")
                    .post(format!("http://{address}/api/auth.test"))
                    .form(&[("token", "xoxc-secret")])
                    .send(),
            )
            .expect_err("cross-origin redirect must not be followed");
        assert!(error.is_redirect(), "{error}");
        handled.join().expect("mock server should finish");
    }

    #[test]
    fn redirect_validation_blocks_ssrf_and_permits_safe_targets() {
        let public_initial = Url::parse("https://files.slack.com/files-pri/T1/a.png").unwrap();
        let public_next = Url::parse("https://cdn.slack-edge.com/a.png").unwrap();
        let http_downgrade = Url::parse("http://cdn.slack-edge.com/a.png").unwrap();
        let loopback_target = Url::parse("http://127.0.0.1:8080/admin").unwrap();
        let metadata_target = Url::parse("https://169.254.169.254/latest/meta-data/").unwrap();
        let local_domain = Url::parse("https://service.internal/secret").unwrap();
        let credentialed = Url::parse("https://user:pass@cdn.slack-edge.com/a.png").unwrap();

        assert!(is_allowed_redirect_target(
            &public_next,
            Some(&public_initial)
        ));
        assert!(!is_allowed_redirect_target(
            &http_downgrade,
            Some(&public_initial)
        ));
        assert!(!is_allowed_redirect_target(
            &loopback_target,
            Some(&public_initial)
        ));
        assert!(!is_allowed_redirect_target(
            &metadata_target,
            Some(&public_initial)
        ));
        assert!(!is_allowed_redirect_target(
            &local_domain,
            Some(&public_initial)
        ));
        assert!(!is_allowed_redirect_target(
            &credentialed,
            Some(&public_initial)
        ));

        let loopback_initial = Url::parse("http://127.0.0.1:12345/start").unwrap();
        let loopback_hop = Url::parse("http://localhost:12345/next").unwrap();
        assert!(is_allowed_redirect_target(
            &loopback_hop,
            Some(&loopback_initial)
        ));
    }
}
