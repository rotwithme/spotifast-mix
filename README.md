# Spotifast Mix

A native desktop app combining Spotifast's interface with Spotify and SoundCloud.
This is an independent custom version of [Spotifast](https://github.com/crmne/spotifast),
written in Rust with egui and fastframe. It has no embedded browser, telemetry,
or app-operated backend.

Spotify audio comes from Spotify through librespot. SoundCloud audio comes from
SoundCloud's official API. Songs always show their original service.

## Listening

- Search both services, or filter results to Spotify or SoundCloud.
- Connect either account independently. Spotify playback requires Spotify Premium.
- Browse SoundCloud likes and playlists, like or unlike tracks, and edit playlists you own.
- Create local playlists containing songs from both services.
- Open local playlists alongside Spotify playlists in the sidebar's Playlists
  list. Search, sort, pin, or drag them into your preferred library order.
- Add selected songs to a new or existing local playlist. Right-click a Spotify
  playlist and choose **Merge into local playlist** to copy its complete contents.
- Play a mixed queue on this computer through **Local mix**, or use the existing
  **Spotify Connect** player. Their queues remain independent.

See [Getting started and account setup](docs/_reference/spotifast-mix.md) for
credentials, playback behavior, storage, and limitations.

## Build on Windows

Use the Rust version pinned in `rust-toolchain.toml`, the MSVC C++ build tools,
and a Windows SDK. Build the native app without the optional MilkDrop visualizer:

```powershell
cargo build --release --locked --bin spotifast-mix --no-default-features
.\target\release\spotifast-mix.exe
```

A default-feature build also includes MilkDrop and requires the CMake, libclang,
and vcpkg dependencies described in [CONTRIBUTING.md](CONTRIBUTING.md).
The underlying app remains cross-platform; a successful Windows build does not
establish Linux or macOS runtime coverage.

## Share a portable Windows build

```powershell
powershell -ExecutionPolicy Bypass -File packaging/windows/package-mix.ps1 -WithoutMilkDrop
```

The script builds a release executable, checks its identity, and writes a ZIP
and SHA-256 checksum under `dist/`. Use `-NoBuild` to package an already built
release. Unpack the ZIP into a writable folder before opening it.

The package includes a portable marker, never your profile, credentials,
or account data. Each recipient signs into their own accounts and supplies their
own SoundCloud API app credentials. The first launch creates `spotifast-mix-data`
next to the executable. It does not read an upstream Spotifast profile.

Inherited Windows installers are disabled. This version has separate app data,
credentials, IPC, and desktop identity, and cannot install upstream Spotifast updates.

## Updates

Windows x64 portable builds check [this project's GitHub releases](https://github.com/rotwithme/spotifast-mix/releases).
Checks run at startup and every two minutes while the app is running, including
when idle or in the tray. A new stable release normally appears within two
minutes, subject to connectivity and GitHub availability.
A new build appears in the existing update icon. Open it, download the update,
and choose **Restart to update**. The app verifies the publisher signature and
download checksum, then replaces its executable while preserving accounts,
settings, and playlists. Settings includes **Check for updates** and the existing
automatic check/download options.

The earlier 0.14.2 custom builds had updates disabled. Upgrade them once with
`packaging/windows/bootstrap-mix.ps1` from this repository, or replace their EXE
with the current portable release. The bootstrap script finds a running portable
copy, waits for you to quit it from the tray, and preserves its data folder.
Future builds use the in-app update flow.

To publish an update, bump the version and release notes, push `main`, wait for
its CI checks, then push the matching `v` version tag. The Windows release
workflow builds, signs, and publishes that version in this repository.

## Before release

Live SoundCloud authorization, stream playback and seeking, token renewal, and
account operations require testing with your registered app. SoundCloud API app
registration currently requires Artist Pro. Native alternative listening services
and mixed-service playback also require appropriate permission under
[SoundCloud's API terms](https://developers.soundcloud.com/docs/api/terms-of-use).
App registration alone does not establish that permission.

No published download or completed runtime-validation claim is implied by this source.

## Credits

Spotifast Mix retains Spotifast's MIT license and copyright notices.
[BetterSoundCloud](https://github.com/AlirezaKJ/BetterSoundCloud) informed the
integration research. Its Electron application is not embedded in this native app.
