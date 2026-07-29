use reqwest::ClientBuilder;

pub(crate) fn ensure_tls_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let installation = rustls::crypto::aws_lc_rs::default_provider().install_default();
        debug_assert!(
            installation.is_ok() || rustls::crypto::CryptoProvider::get_default().is_some()
        );
    }
}

pub(crate) fn builder() -> ClientBuilder {
    ensure_tls_crypto_provider();
    reqwest::Client::builder()
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
}
