---
title: Spotifast Mix
description: Accounts, local mixed playback, storage, and building this custom version.
---

# Spotifast Mix

Spotifast Mix is a custom version of Spotifast with a second, native SoundCloud
integration. It keeps the existing Spotify client and Spotify Connect controls.
It does not substitute SoundCloud recordings for Spotify tracks.

## Connect accounts

You can connect Spotify, SoundCloud, or both. Each service has its own credentials,
connection state, library, and errors. A SoundCloud account does not require a
Spotify account. Local Spotify playback still requires Spotify Premium and its
separate playback approval.

For Spotify, create your own Spotify developer app and register the Web API
redirect URI `http://127.0.0.1:8989/login`. Complete the separate playback setup
when you want Spotify songs to play on this computer. Its registered playback
callback uses port 8898. Another app authorizing on that port must finish first.

For SoundCloud:

1. Register your own application through the
   [SoundCloud developer tools](https://developers.soundcloud.com/docs/api/register-app).
   SoundCloud currently lists Artist Pro as a registration prerequisite.
2. Register exactly `http://127.0.0.1:8899/soundcloud/callback` as its redirect URI.
3. Enter your Client ID and secret locally in the SoundCloud connection form.
   Do not send them in a chat, log, screenshot, issue, or shared package.
4. Choose **Connect with SoundCloud** and approve access in your normal browser.
   The app verifies the callback state and completes the PKCE authorization flow.

If port 8899 is occupied, close the application using it and retry. The callback
is loopback-only; the app does not run a public authorization service.

Durable grants and the SoundCloud app secret use the platform's protected store.
Signing out of one service leaves the other connected. Disconnecting SoundCloud
revokes this app's access through `/disconnect`, clears its account caches, and
invalidates pending account requests. It does not sign you out of soundcloud.com.

## Search and libraries

Search defaults to **All**, with **Spotify** and **SoundCloud** filters. Each
service can finish loading or report an error independently. Tracks and playlists
show their source. SoundCloud rows credit their uploader and link to the original
work. Existing Spotify-only categories keep their current behavior.

SoundCloud supports likes and owned playlist management. Adding, removing, or
reordering tracks updates that SoundCloud playlist only. An external playlist
change can conflict with an edit; reload before trying again rather than
replacing another editor's changes. The API has no atomic playlist-revision guard,
so a simultaneous edit between the final read and write cannot be fully prevented.

A local mixed playlist is stored on this computer. It may contain Spotify and
SoundCloud references in any order, including repeated songs. It is not uploaded
to either service. Spotify playlists accept Spotify songs, and SoundCloud playlists
accept SoundCloud songs. Inaccessible tracks stay identifiable as unavailable
entries rather than being replaced by another recording.

## Spotify Connect and Local mix

The player has independent **Spotify Connect** and **Local mix** sessions.

- Existing Spotify Play commands keep their current behavior. Use **Play in local
  mix** when you want a Spotify song in the mixed session.
- SoundCloud Play and local mixed-playlist Play start Local mix on this computer.
- **Add to local mix** and **Play next in local mix** update its queue immediately.
  Adding songs while Spotify Connect plays does not interrupt it. Start the mix
  explicitly when ready.
- Starting Local mix pauses the Spotify playback currently controlled by the app.
  It does not copy, clear, or transfer Spotify's queue.
- Switching back pauses Local mix and shows Spotify's current session. Its Play
  control resumes that session. The local mixed queue and position remain saved.
- Incoming Spotify Connect playback on this computer pauses Local mix and selects
  Spotify Connect. Spotify devices cannot receive SoundCloud songs or mixed queues.

Local mix follows the existing queue rules: manually queued songs precede the
remaining collection, duplicate songs represent separate occurrences, Next consumes
one occurrence immediately, and Clear removes manual songs without clearing the
collection. Older backend responses cannot undo newer queue actions.

Both providers use the native EQ and output chain. Visualizers show the signal
after EQ and before volume, so muting does not stop their motion.

## SoundCloud playback limits

SoundCloud streams use AAC HLS from the official API, preferring the offered
160 kbps stream and using 96 kbps when necessary. Seeking and decoding happen off
the UI thread. Signed stream URLs and audio buffers are session-only.

Protected API stream links use the SoundCloud account's authorization. Redirects
are checked individually, and account tokens are never forwarded to audio CDN
hosts. An API token rejected with HTTP 401 is refreshed once; a CDN rejection
does not trigger an account-token refresh.

A track can be playable, preview-only, or blocked by the service. Previews are
labelled as previews. Missing API streams, territorial restrictions, paywalls,
unsupported formats, and DRM-protected content are reported without scraping,
circumvention, or replacement audio. SoundCloud audio is not available offline.

The SoundCloud API's terms restrict alternative listening services and playback
combining content from multiple services unless explicitly licensed. Appropriate
permission and live API validation are release prerequisites for this integration.
See [API terms](https://developers.soundcloud.com/docs/api/terms-of-use) and
[streaming formats](https://developers.soundcloud.com/blog/api-streaming-urls/).

## Files and privacy

Spotifast Mix uses a distinct app identity, credential service, instance slot,
window identity, and `spotifast-mix:` link scheme. It does not register itself as
the default handler for `spotify:` or generic web links. Paste either provider's
original links in the app, or use `spotifast-mix:<provider-link>` with a registered
launcher.

On Windows the normal per-user profile is under the `spotifastmix/spotifast-mix`
organization/application folders in AppData. A portable ZIP uses
`spotifast-mix-data/config`, `spotifast-mix-data/state`, and
`spotifast-mix-data/cache` beside the executable instead. Profiles and protected
credential entries remain separate from upstream Spotifast. Fresh sign-ins are
required. Do not share your data directory when sharing the app.

Local mixed-playlist files store user-authored details and ordered provider
references. Session files store the mixed queue and playback position. SoundCloud
metadata is fetched again each session; signed stream URLs, audio, artwork caches,
and access tokens are not written into these JSON files. Durable files use the
app's atomic background writer.

SoundCloud requests go to its authorization/API endpoints and the stream and
artwork hosts returned by the API. They use the configured proxy. Existing Spotify
network behavior remains documented in [How it connects](how-it-connects.md).
The custom app has no telemetry or hosted backend. Automatic and manual update
installation are disabled until a separate release repository and signing key exist.

## Build and distribute

The initial supported delivery is a portable Windows ZIP. Follow the repository
README and `packaging/windows/package-mix.ps1`; the helper never packages local
profiles or credentials. The executable reports `spotifast-mix <version>`.

Linux and macOS remain source/build targets. Distribution templates use separate
identities but require a real fork publication destination before use. Inherited
Windows MSI and Inno installers are intentionally blocked, including their direct
compiler entrypoints, so they cannot take over an upstream installation.

The optional Omarchy hook imports `spotifast-mix.json` into
`$XDG_CONFIG_HOME/spotifast-mix/themes/omarchy.json` and calls only
`spotifast-mix reload-themes`. Its overrides are
`SPOTIFAST_MIX_OMARCHY_THEME_DIR` and `SPOTIFAST_MIX_THEMES_DIR`.
The fork never modifies the upstream palette folder or contacts an upstream instance.

Inherited release, package publication, documentation deployment, and Flatpak
publication workflows are explicitly disabled. The main CI still builds, tests,
and checks portable ZIPs, macOS bundles, and the Nix package. Re-enabling dormant
publication recipes requires a configured fork destination and identity review.

A source build or successful compile does not verify live SoundCloud access or
runtime playback on another platform. Release validation must record actual tested
platforms, authorization, seeking, stream renewal, playlist writes, credential-store
round trips, and matching visual evidence in light/dark themes at narrow/normal sizes.

## Deterministic interface checks

The existing Spotify interface keeps its translated catalogs. The new SoundCloud
and Local mix controls currently use English; selecting another language does not
translate those new surfaces yet.

With the `demo` feature, `--demo-page mix` seeds invented SoundCloud accounts,
tracks, likes, playlists, and a mixed queue. It never loads real credentials or
audio streams. Additional surfaces are `mix-library`, `mix-queue`, `mix-playlist`,
`soundcloud-search`, `soundcloud-playlist`, `soundcloud-only`, `mix-error`,
`mix-loading`, and `mix-accounts`. Existing Spotify fixture pages remain unchanged
unless a mixed surface is selected.

For example, capture the local mixed queue in a light window:

```powershell
cargo run --locked --no-default-features --features demo --bin spotifast-mix -- --demo --demo-page mix --demo-show mix-queue,light --demo-size 1200x800 --demo-shot .cache/demo-mix.png
```

Repeat with `dark` and `760x800` for the other representative theme and size.
Keep captures and comparison pages outside tracked source, and label new surfaces
that have no direct upstream Before equivalent.
