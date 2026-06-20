use std::path::PathBuf;

pub const APPLICATION_ID: &str = "eu.vanadrighem.conduit";
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

pub fn webkit_data_dir() -> PathBuf {
    app_cache_dir().join("webkit-data")
}

pub fn webkit_cache_dir() -> PathBuf {
    app_cache_dir().join("webkit-cache")
}

pub fn image_asset_cache_dir() -> PathBuf {
    app_cache_dir().join("image-assets")
}

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

fn user_cache_dir() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache", "conduit-cache")
}

fn app_cache_dir() -> PathBuf {
    user_cache_dir().join(APPLICATION_ID)
}

fn xdg_dir(env_name: &str, home_suffix: &str, temp_suffix: &str) -> PathBuf {
    std::env::var_os(env_name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(home_suffix))
        })
        .unwrap_or_else(|| std::env::temp_dir().join(temp_suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_cache_paths_live_under_app_cache_dir() {
        let app_cache = app_cache_dir();

        assert!(webkit_data_dir().starts_with(&app_cache));
        assert!(webkit_cache_dir().starts_with(&app_cache));
        assert!(image_asset_cache_dir().starts_with(&app_cache));
    }
}
