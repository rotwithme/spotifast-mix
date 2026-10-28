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
There is no configured public release or package-publishing destination.

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
