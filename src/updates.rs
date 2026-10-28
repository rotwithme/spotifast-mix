//! Signed updates from this fork's own GitHub release channel.

pub use fastframe_update::{
    CHECK_INTERVAL, DownloadState, Installation, Kind, Prepared, Release, Source, Unsupported,
    Updater,
};
use fastframe_update::{MacConfig, ReqwestTransport, UpdateConfig};

pub const ENABLED: bool = cfg!(all(target_os = "windows", target_arch = "x86_64"));
pub const UNAVAILABLE: &str =
    "In-app updates are available for the Windows x64 portable build of Spotifast Mix.";

/// This repository and publisher key belong to Mix. Upstream releases and
/// legacy upstream install identities are never accepted by this updater.
pub const CONFIG: UpdateConfig = UpdateConfig {
    macos: MacConfig {
        bundle_ids: &["app.spotifastmix.SpotifastMix"],
        executable_names: &["Spotifast Mix"],
        legacy_bundle_names: &[],
    },
    publisher_key: Some(include_str!("../assets/update-public-key.hex")),
    appimage: false,
    ..UpdateConfig::new(
        "rotwithme/spotifast-mix",
        "Spotifast Mix",
        "spotifast-mix",
        env!("CARGO_PKG_VERSION"),
    )
};

pub fn updater(proxy: &crate::settings::ProxyConfig) -> anyhow::Result<Updater> {
    anyhow::ensure!(ENABLED, UNAVAILABLE);
    CONFIG.validate()?;
    let builder = crate::http::blocking_builder(proxy).map_err(anyhow::Error::msg)?;
    Ok(Updater::new(CONFIG, ReqwestTransport::new(builder)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_updates_trust_only_the_mix_publisher_and_repository() {
        CONFIG.validate().unwrap();
        assert_eq!(CONFIG.repository, "rotwithme/spotifast-mix");
        assert_eq!(CONFIG.slug, "spotifast-mix");
        assert_eq!(CONFIG.legacy_names, &[] as &[&str]);
        assert_eq!(CONFIG.legacy_windows_installs, &[] as &[&str]);
        let key = CONFIG.publisher_key.unwrap().trim();
        assert_eq!(key.len(), 64);
        assert_ne!(
            key,
            "e44c87b1550484c687947bd927ae333ff431cc7adc6cc33533c628f82016b554"
        );
        const { assert!(!CONFIG.appimage) };
        assert_eq!(CONFIG.current_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn invalid_proxy_cannot_be_bypassed_by_update_requests() {
        let proxy = crate::settings::ProxyConfig::Invalid("invalid update proxy".into());
        assert!(updater(&proxy).is_err());
    }
}
