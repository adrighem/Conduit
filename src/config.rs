pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GETTEXT_PACKAGE: &str = "conduit";
pub const LOCALEDIR: &str = match option_env!("CONDUIT_LOCALEDIR") {
    Some(path) => path,
    None => "/usr/local/share/locale",
};
pub const PKGDATADIR: &str = match option_env!("CONDUIT_PKGDATADIR") {
    Some(path) => path,
    None => "/usr/local/share/conduit",
};

pub fn slack_client_id() -> Option<String> {
    let packaged_client_id = option_env!("CONDUIT_SLACK_CLIENT_ID")
        .map(ToString::to_string)
        .map(|client_id| client_id.trim().to_string())
        .filter(|client_id| !client_id.is_empty());
    packaged_client_id.or_else(|| {
        std::env::var("CONDUIT_SLACK_CLIENT_ID")
            .ok()
            .map(|client_id| client_id.trim().to_string())
            .filter(|client_id| !client_id.is_empty())
    })
}
