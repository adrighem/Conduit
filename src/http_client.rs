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

pub(crate) fn is_public_domain(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host_lower = host.trim_end_matches('.').to_ascii_lowercase();
    if host_lower.is_empty()
        || host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
        || host_lower.ends_with(".internal")
        || host_lower.ends_with(".arpa")
    {
        return false;
    }
    if url
        .host()
        .is_some_and(|h| matches!(h, url::Host::Ipv4(_) | url::Host::Ipv6(_)))
    {
        return false;
    }
    if host_lower.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    host_lower.contains('.')
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

pub(crate) fn builder() -> ClientBuilder {
    ensure_tls_crypto_provider();
    reqwest::Client::builder().redirect(redirect_policy())
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
