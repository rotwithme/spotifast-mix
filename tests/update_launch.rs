//! This custom build has no update publisher and must reject updater helper
//! requests before it opens a profile or starts the application.

use std::process::Command;

#[test]
fn updater_helper_flags_are_rejected_before_startup() {
    for flag in ["--update-receipt", "--update-error", "--apply-update"] {
        let scratch = std::env::temp_dir().join(format!(
            "spotifast-mix-disabled-update-{:016x}",
            rand::random::<u64>()
        ));
        std::fs::create_dir(&scratch).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_spotifast-mix"))
            .arg(flag)
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
        assert_eq!(output.status.code(), Some(2), "{flag}: {output:?}");
        assert!(
            !output.stderr.is_empty(),
            "the argument parser explains the failure"
        );
        assert_eq!(left, 0, "updater flags must not start the app");
    }
}
