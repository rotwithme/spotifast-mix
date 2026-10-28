//! Updater helpers and relaunch flags are handled before app/profile startup.

use std::process::Command;

#[test]
fn update_flags_follow_the_configured_platform_without_starting_the_app() {
    for flags in [
        [
            "--update-receipt",
            "/missing/.spotifast-mix-update-0000000000000000/handoff.json",
        ],
        ["--update-error", "The previous version has been restored."],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_spotifast-mix"))
            .args(flags)
            .arg("--version")
            .output()
            .unwrap();
        if spotifast::updates::ENABLED {
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                format!("spotifast-mix {}", env!("CARGO_PKG_VERSION"))
            );
        } else {
            assert_eq!(output.status.code(), Some(2));
        }
    }
}

#[test]
fn apply_update_never_opens_an_app_or_profile_for_a_missing_job() {
    let scratch = std::env::temp_dir().join(format!(
        "spotifast-mix-apply-update-{:016x}",
        rand::random::<u64>()
    ));
    std::fs::create_dir(&scratch).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_spotifast-mix"))
        .arg("--apply-update")
        .arg(scratch.join("missing-job.json"))
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        .env("XDG_STATE_HOME", scratch.join("state"))
        .env("XDG_CACHE_HOME", scratch.join("cache"))
        .env("XDG_DATA_HOME", scratch.join("data"))
        .env("APPDATA", scratch.join("appdata"))
        .env("LOCALAPPDATA", scratch.join("localappdata"))
        .output()
        .unwrap();
    let left = std::fs::read_dir(&scratch).unwrap().count();
    std::fs::remove_dir_all(&scratch).unwrap();
    assert_eq!(
        output.status.code(),
        Some(if spotifast::updates::ENABLED { 1 } else { 2 }),
        "{output:?}"
    );
    assert!(!output.stderr.is_empty());
    assert_eq!(left, 0, "helper flags must not start the application");
}
