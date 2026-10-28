//! Update types retained for the existing interface, with installation disabled.
//!
//! Spotifast Mix has no release repository or signing key yet. Constructing an
//! updater must fail before a transport is created, so neither automatic nor
//! manual checks can replace this fork with upstream Spotifast.

pub use fastframe_update::{
    CHECK_INTERVAL, DownloadState, Installation, Kind, Prepared, Release, Source, Unsupported,
    Updater,
};
use fastframe_update::{MacConfig, UpdateConfig};

pub const ENABLED: bool = false;
pub const UNAVAILABLE: &str =
    "Updates are not configured for Spotifast Mix. Install a new build manually.";

/// Compatibility metadata only. No updater is constructed from this config.
/// A real fork publisher and its own signing key are required before enabling it.
pub const CONFIG: UpdateConfig = UpdateConfig {
    macos: MacConfig {
        bundle_ids: &["app.spotifastmix.SpotifastMix"],
        executable_names: &["Spotifast Mix"],
        legacy_bundle_names: &[],
    },
    publisher_key: None,
    appimage: false,
    ..UpdateConfig::new(
        "unconfigured/spotifast-mix",
        "Spotifast Mix",
        "spotifast-mix",
        env!("CARGO_PKG_VERSION"),
    )
};

pub fn updater(_proxy: &crate::settings::ProxyConfig) -> anyhow::Result<Updater> {
    anyhow::bail!(UNAVAILABLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_has_no_upstream_update_path() {
        const { assert!(!ENABLED) };
        assert_eq!(CONFIG.slug, "spotifast-mix");
        assert!(CONFIG.publisher_key.is_none());
        const { assert!(!CONFIG.appimage) };
        assert_eq!(CONFIG.current_version, env!("CARGO_PKG_VERSION"));
        let Err(error) = updater(&crate::settings::ProxyConfig::default()) else {
            panic!("an unconfigured fork must never construct an updater");
        };
        assert_eq!(error.to_string(), UNAVAILABLE);
    }
}
