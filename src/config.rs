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
