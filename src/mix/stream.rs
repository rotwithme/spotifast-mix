//! Bounded, memory-only SoundCloud HLS transport and native AAC decoding.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use librespot_playback::config::VolumeCtrl;
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::VolumeGetter;
use librespot_playback::mixer::mappings::{CubicMapping, VolumeMapping};
use mpeg2ts_reader::{StreamType, demultiplex, packet, pes, psi};
use reqwest::Url;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::mpsc;

use crate::player::EngineConfig;
use crate::resample::Resampler;
use crate::sink::{AudioControl, RodioSink};
use crate::vis::Tapped;

const MAX_PLAYLIST: usize = 4 * 1024 * 1024;
const MAX_SEGMENT: usize = 16 * 1024 * 1024;
const PCM_CHUNK: usize = 4096;

pub(super) enum StreamEvent {
    Playing(u32),
    Position(u32),
    Ended,
    Error(String),
}

pub(super) type Notify = Arc<dyn Fn(StreamEvent) + Send + Sync>;

struct Control {
    cancelled: AtomicBool,
    paused: Mutex<bool>,
    changed: Condvar,
    volume: AtomicU16,
}

#[derive(Clone)]
struct Volume(Arc<Control>);

impl VolumeGetter for Volume {
    fn attenuation_factor(&self) -> f64 {
        let volume = self.0.volume.load(Ordering::Relaxed);
        if volume == 0 {
            0.0
        } else if volume == u16::MAX {
            1.0
        } else {
            CubicMapping::linear_to_mapped(
                f64::from(volume) / f64::from(u16::MAX),
                VolumeCtrl::DEFAULT_DB_RANGE,
            )
        }
    }
}

pub(super) struct SoundCloudPlayback {
    control: Arc<Control>,
    download: tokio::task::JoinHandle<()>,
    decoder: Option<tokio::task::JoinHandle<()>>,
}

impl SoundCloudPlayback {
    pub fn spawn(
        config: EngineConfig,
        client: Arc<crate::soundcloud::Client>,
        urn: String,
        position_ms: u32,
        playing: bool,
        notify: Notify,
    ) -> Self {
        let control = Arc::new(Control {
            cancelled: AtomicBool::new(false),
            paused: Mutex::new(!playing),
            changed: Condvar::new(),
            volume: AtomicU16::new(config.initial_volume),
        });
        // Two compressed segments are the only transport read-ahead. The
        // decoder itself is paced by RodioSink's 200 ms output bound.
        let (tx, rx) = mpsc::channel(2);
        let transport_control = Arc::clone(&control);
        let download = tokio::spawn(async move {
            let result = download(&client, &urn, position_ms, &tx, &transport_control).await;
            if let Err(error) = result
                && !transport_control.cancelled.load(Ordering::Acquire)
            {
                let _ = tx.send(Err(error.to_string())).await;
            }
        });
        let decoder_control = Arc::clone(&control);
        let decoder = tokio::task::spawn_blocking(move || {
            let result = decode(config, rx, position_ms, &decoder_control, &notify);
            if decoder_control.cancelled.load(Ordering::Acquire) {
                return;
            }
            match result {
                Ok(()) => notify(StreamEvent::Ended),
                Err(error) => notify(StreamEvent::Error(error.to_string())),
            }
        });
        Self {
            control,
            download,
            decoder: Some(decoder),
        }
    }

    pub fn pause(&self, paused: bool) {
        *self
            .control
            .paused
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = paused;
        self.control.changed.notify_all();
    }

    pub fn volume(&self, volume: u16) {
        self.control.volume.store(volume, Ordering::Relaxed);
    }

    pub async fn stop(mut self) {
        self.cancel();
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.await;
        }
    }

    fn cancel(&self) {
        self.control.cancelled.store(true, Ordering::Release);
        // Synchronize with Condvar::wait so a paused decoder cannot miss the
        // cancellation notification between checking the flag and sleeping.
        *self
            .control
            .paused
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = false;
        self.control.changed.notify_all();
        self.download.abort();
    }
}

impl Drop for SoundCloudPlayback {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug)]
struct Chunk {
    bytes: Vec<u8>,
    /// The first selected segment may precede a seek by a few seconds.
    skip_ms: u32,
}

#[derive(Debug)]
struct Segment {
    url: Url,
    duration_ms: u32,
    range: Option<(u64, u64)>,
    discontinuity: bool,
    map: Option<(Url, Option<(u64, u64)>)>,
}

#[derive(Debug)]
struct Playlist {
    variant: Option<Url>,
    map: Option<(Url, Option<(u64, u64)>)>,
    segments: Vec<Segment>,
}

fn attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = line;
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        if let Some(quoted) = after.strip_prefix('"') {
            let (value, tail) = quoted.split_once('"')?;
            if key.trim() == name {
                return Some(value);
            }
            rest = tail.strip_prefix(',').unwrap_or(tail);
        } else {
            let (value, tail) = after.split_once(',').unwrap_or((after, ""));
            if key.trim() == name {
                return Some(value);
            }
            rest = tail;
        }
    }
    None
}

fn resolve(base: &Url, value: &str) -> Result<Url> {
    let url = base
        .join(value)
        .map_err(|_| anyhow!("SoundCloud returned an invalid stream address"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("SoundCloud returned an unsupported stream address");
    }
    Ok(url)
}

fn byte_range(value: &m3u8_rs::ByteRange, previous_end: u64) -> Result<(u64, u64)> {
    let length = value.length;
    let offset = value.offset.unwrap_or(previous_end);
    if length == 0 || length > MAX_SEGMENT as u64 || offset.checked_add(length).is_none() {
        bail!("SoundCloud stream segment is too large");
    }
    Ok((offset, length))
}

fn parse_playlist(base: &Url, text: &str) -> Result<Playlist> {
    if !text.trim_start().starts_with("#EXTM3U") {
        bail!("SoundCloud returned an invalid HLS playlist");
    }
    let mut playlist = Playlist {
        variant: None,
        map: None,
        segments: Vec::new(),
    };
    // Refuse encrypted or malformed encryption declarations even if the
    // tolerant manifest parser would otherwise preserve one as an unknown tag.
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(attributes) = line
            .strip_prefix("#EXT-X-KEY:")
            .or_else(|| line.strip_prefix("#EXT-X-SESSION-KEY:"))
            && attribute(attributes, "METHOD") != Some("NONE")
        {
            bail!("This SoundCloud stream uses encryption that Local mix does not support");
        }
    }
    let parsed = m3u8_rs::parse_playlist_res(text.trim_start().as_bytes())
        .map_err(|_| anyhow!("SoundCloud returned an invalid HLS playlist"))?;
    match parsed {
        m3u8_rs::Playlist::MasterPlaylist(master) => {
            if master
                .session_key
                .iter()
                .any(|key| key.0.method != m3u8_rs::KeyMethod::None)
            {
                bail!("Encrypted SoundCloud streams are not supported");
            }
            let variant = master
                .variants
                .iter()
                .filter(|variant| !variant.is_i_frame && variant.resolution.is_none())
                .max_by_key(|variant| variant.average_bandwidth.unwrap_or(variant.bandwidth))
                .context("SoundCloud stream has no audio rendition")?;
            playlist.variant = Some(resolve(base, &variant.uri)?);
        }
        m3u8_rs::Playlist::MediaPlaylist(media) => {
            let mut previous_end = 0;
            let mut current_map = None;
            for segment in media.segments {
                if !segment.duration.is_finite()
                    || segment.duration <= 0.0
                    || segment.duration > 3600.0
                {
                    bail!("Invalid HLS segment duration");
                }
                if let Some(map) = segment.map {
                    let range = map
                        .byte_range
                        .as_ref()
                        .map(|range| byte_range(range, 0))
                        .transpose()?;
                    current_map = Some((resolve(base, &map.uri)?, range));
                    if playlist.segments.is_empty() {
                        playlist.map = current_map.clone();
                    }
                }
                let range = segment
                    .byte_range
                    .as_ref()
                    .map(|range| byte_range(range, previous_end))
                    .transpose()?;
                if let Some((offset, length)) = range {
                    previous_end = offset + length;
                }
                playlist.segments.push(Segment {
                    url: resolve(base, &segment.uri)?,
                    duration_ms: (f64::from(segment.duration) * 1000.0).round() as u32,
                    range,
                    discontinuity: segment.discontinuity,
                    map: current_map.clone(),
                });
            }
        }
    }
    if playlist.variant.is_none() && playlist.segments.is_empty() {
        bail!("SoundCloud stream has no audio segments");
    }
    Ok(playlist)
}

async fn fetch(
    client: &crate::soundcloud::Client,
    url: Url,
    range: Option<(u64, u64)>,
    limit: usize,
) -> Result<(Url, Vec<u8>)> {
    let mut response = client.stream_response(url, range).await?;
    let status = response.status();
    if !status.is_success() {
        bail!("SoundCloud stream request failed ({})", status.as_u16());
    }
    if range.is_some() && status != reqwest::StatusCode::PARTIAL_CONTENT {
        bail!("SoundCloud server did not honor the stream byte range");
    }
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        bail!("SoundCloud stream response is too large");
    }
    let final_url = response.url().clone();
    if final_url.scheme() != "https" {
        bail!("SoundCloud redirected the stream to an unsupported address");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow!("Stream read failed: {}", error.without_url()))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            bail!("SoundCloud stream response is too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((final_url, bytes))
}

async fn download(
    client: &crate::soundcloud::Client,
    urn: &str,
    position_ms: u32,
    tx: &mpsc::Sender<Result<Chunk, String>>,
    control: &Control,
) -> Result<()> {
    let streams = client.streams(urn).await?;
    let address = streams
        .preferred()
        .context("SoundCloud does not offer a full playable stream for this track")?;
    let mut url = Url::parse(address)
        .map_err(|_| anyhow!("SoundCloud returned an invalid stream address"))?;
    url = resolve(&url, address)?;
    let mut selected = None;
    for _ in 0..4 {
        let (base, bytes) = fetch(client, url, None, MAX_PLAYLIST).await?;
        let text =
            std::str::from_utf8(&bytes).context("SoundCloud returned an invalid HLS playlist")?;
        let playlist = parse_playlist(&base, text)?;
        if let Some(variant) = playlist.variant {
            url = variant;
        } else {
            selected = Some(playlist);
            break;
        }
    }
    let playlist = selected.context("SoundCloud HLS playlist nesting is too deep")?;
    let mut normalizer = Normalizer::default();
    let mut current_map = playlist.map;
    if let Some((url, range)) = current_map.clone() {
        let (_, bytes) = fetch(client, url, range, MAX_SEGMENT).await?;
        normalizer.initialize(&bytes)?;
    }
    let mut elapsed = 0u32;
    let mut first = true;
    for segment in playlist.segments {
        if control.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if segment.duration_ms > 0 && elapsed.saturating_add(segment.duration_ms) <= position_ms {
            elapsed = elapsed.saturating_add(segment.duration_ms);
            continue;
        }
        let (_, bytes) = fetch(client, segment.url, segment.range, MAX_SEGMENT).await?;
        if segment.map != current_map {
            if let Some((url, range)) = segment.map.clone() {
                let (_, initialization) = fetch(client, url, range, MAX_SEGMENT).await?;
                normalizer.initialize(&initialization)?;
            }
            current_map = segment.map;
        }
        if segment.discontinuity {
            normalizer.transport = None;
        }
        let bytes = normalizer.segment(bytes)?;
        let skip_ms = if first {
            position_ms.saturating_sub(elapsed)
        } else {
            0
        };
        first = false;
        tx.send(Ok(Chunk { bytes, skip_ms }))
            .await
            .map_err(|_| anyhow!("Playback was cancelled"))?;
        elapsed = elapsed.saturating_add(segment.duration_ms);
    }
    Ok(())
}

#[derive(Default)]
struct Normalizer {
    aac: Option<AacConfig>,
    transport: Option<Transport>,
}

#[derive(Clone, Copy)]
struct AacConfig {
    profile: u8,
    frequency: u8,
    channels: u8,
    default_size: u32,
}

impl Normalizer {
    fn initialize(&mut self, bytes: &[u8]) -> Result<()> {
        // mp4a sample descriptions carry the decoder-specific AudioSpecificConfig
        // in esds. Convert unencrypted fMP4 AAC samples to ADTS, which the same
        // streaming native decoder can consume as packed AAC and MPEG-TS AAC.
        let esds = bytes
            .windows(4)
            .position(|value| value == b"esds")
            .context("Unsupported SoundCloud HLS initialization format")?;
        let atom_start = esds.checked_sub(4).context("Invalid MP4 initialization")?;
        let size = be32(bytes, atom_start)? as usize;
        let end = atom_start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .context("Invalid MP4 initialization size")?;
        let body = bytes
            .get(esds + 8..end)
            .context("Invalid MP4 decoder configuration")?;
        let default_size = bytes
            .windows(4)
            .position(|value| value == b"trex")
            .and_then(|at| be32(bytes, at + 20).ok())
            .unwrap_or(0);
        for index in 0..body.len().saturating_sub(3) {
            if body[index] != 5 {
                continue;
            }
            let mut at = index + 1;
            let mut length = 0usize;
            let mut valid = false;
            for _ in 0..4 {
                let Some(byte) = body.get(at) else {
                    break;
                };
                at += 1;
                length = (length << 7) | usize::from(byte & 0x7f);
                if byte & 0x80 == 0 {
                    valid = true;
                    break;
                }
            }
            if !valid || !(2..=64).contains(&length) || at + length > body.len() {
                continue;
            }
            let object = body[at] >> 3;
            let frequency = ((body[at] & 7) << 1) | (body[at + 1] >> 7);
            let channels = (body[at + 1] >> 3) & 15;
            if object == 2 && frequency < 13 && (1..=7).contains(&channels) {
                self.aac = Some(AacConfig {
                    profile: object - 1,
                    frequency,
                    channels,
                    default_size,
                });
                return Ok(());
            }
        }
        bail!("Unsupported SoundCloud AAC configuration")
    }

    fn segment(&mut self, bytes: Vec<u8>) -> Result<Vec<u8>> {
        if let Some(config) = self.aac {
            return fragmented_aac(&bytes, config);
        }
        if self.transport.is_some()
            || (bytes.first() == Some(&0x47)
                && (bytes.len() == 188 || bytes.get(188) == Some(&0x47)))
        {
            return self
                .transport
                .get_or_insert_with(Transport::new)
                .push(&bytes);
        }
        Ok(bytes)
    }
}

fn be32(bytes: &[u8], at: usize) -> Result<u32> {
    let value: [u8; 4] = bytes
        .get(at..at + 4)
        .context("Truncated MP4 stream")?
        .try_into()?;
    Ok(u32::from_be_bytes(value))
}

fn boxes(bytes: &[u8]) -> Result<Vec<(&[u8], &[u8])>> {
    let mut result = Vec::new();
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let short = be32(bytes, at)?;
        let (size, header) = if short == 1 {
            let large: [u8; 8] = bytes
                .get(at + 8..at + 16)
                .context("Truncated MP4 box")?
                .try_into()?;
            (usize::try_from(u64::from_be_bytes(large))?, 16)
        } else if short == 0 {
            (bytes.len() - at, 8)
        } else {
            (short as usize, 8)
        };
        let end = at
            .checked_add(size)
            .filter(|end| *end <= bytes.len() && size >= header)
            .context("Invalid MP4 box size")?;
        result.push((&bytes[at + 4..at + 8], &bytes[at + header..end]));
        at = end;
    }
    Ok(result)
}

fn fragmented_aac(bytes: &[u8], config: AacConfig) -> Result<Vec<u8>> {
    let mut sizes = Vec::new();
    let mut media = Vec::new();
    for (kind, body) in boxes(bytes)? {
        if kind == b"mdat" {
            media.extend_from_slice(body);
        }
        if kind != b"moof" {
            continue;
        }
        for (kind, traf) in boxes(body)? {
            if kind != b"traf" {
                continue;
            }
            let children = boxes(traf)?;
            let mut default_size = config.default_size;
            for (kind, body) in &children {
                if *kind != b"tfhd" {
                    continue;
                }
                let flags = be32(body, 0)? & 0x00ff_ffff;
                let mut at = 8;
                if flags & 1 != 0 {
                    at += 8;
                }
                if flags & 2 != 0 {
                    at += 4;
                }
                if flags & 8 != 0 {
                    at += 4;
                }
                if flags & 16 != 0 {
                    default_size = be32(body, at)?;
                }
            }
            for (kind, body) in children {
                if kind != b"trun" {
                    continue;
                }
                let flags = be32(body, 0)? & 0x00ff_ffff;
                let count = be32(body, 4)?;
                if count > 100_000 {
                    bail!("SoundCloud audio fragment has too many frames");
                }
                let mut at = 8;
                if flags & 1 != 0 {
                    at += 4;
                }
                if flags & 4 != 0 {
                    at += 4;
                }
                for _ in 0..count {
                    if flags & 0x100 != 0 {
                        at += 4;
                    }
                    let size = if flags & 0x200 != 0 {
                        let size = be32(body, at)?;
                        at += 4;
                        size
                    } else {
                        default_size
                    };
                    if flags & 0x400 != 0 {
                        at += 4;
                    }
                    if flags & 0x800 != 0 {
                        at += 4;
                    }
                    if size == 0 || size > 8184 || at > body.len() {
                        bail!("Invalid SoundCloud AAC frame size");
                    }
                    sizes.push(size as usize);
                }
            }
        }
    }
    if sizes.is_empty() {
        bail!("SoundCloud fragment contains no AAC frames");
    }
    let mut output = Vec::with_capacity(media.len() + sizes.len() * 7);
    let mut at = 0;
    for size in sizes {
        let frame = media
            .get(at..at + size)
            .context("Truncated SoundCloud AAC frame")?;
        let length = size + 7;
        output.extend_from_slice(&[
            0xff,
            0xf1,
            (config.profile << 6) | (config.frequency << 2) | (config.channels >> 2),
            ((config.channels & 3) << 6) | ((length >> 11) as u8),
            (length >> 3) as u8,
            ((length & 7) as u8) << 5 | 0x1f,
            0xfc,
        ]);
        output.extend_from_slice(frame);
        at += size;
    }
    Ok(output)
}

mpeg2ts_reader::packet_filter_switch! {
    AudioFilter<TransportContext> {
        Pat: demultiplex::PatPacketFilter<TransportContext>,
        Pmt: demultiplex::PmtPacketFilter<TransportContext>,
        Aac: pes::PesPacketFilter<TransportContext, AacConsumer>,
        Ignore: demultiplex::NullPacketFilter<TransportContext>,
    }
}

pub struct TransportContext {
    changes: demultiplex::FilterChangeset<AudioFilter>,
    audio: Vec<u8>,
    selected: Option<packet::Pid>,
    failed: bool,
}

impl demultiplex::DemuxContext for TransportContext {
    type F = AudioFilter;
    fn filter_changeset(&mut self) -> &mut demultiplex::FilterChangeset<Self::F> {
        &mut self.changes
    }
    fn construct(&mut self, request: demultiplex::FilterRequest<'_, '_>) -> Self::F {
        match request {
            demultiplex::FilterRequest::ByPid(psi::pat::PAT_PID) => {
                AudioFilter::Pat(Default::default())
            }
            demultiplex::FilterRequest::Pmt {
                pid,
                program_number,
            } => AudioFilter::Pmt(demultiplex::PmtPacketFilter::new(pid, program_number)),
            demultiplex::FilterRequest::ByStream {
                stream_type: StreamType::ADTS,
                stream_info,
                ..
            } if self
                .selected
                .is_none_or(|pid| pid == stream_info.elementary_pid()) =>
            {
                self.selected = Some(stream_info.elementary_pid());
                AudioFilter::Aac(pes::PesPacketFilter::new(AacConsumer))
            }
            _ => AudioFilter::Ignore(Default::default()),
        }
    }
}

pub struct AacConsumer;

impl pes::ElementaryStreamConsumer<TransportContext> for AacConsumer {
    fn start_stream(&mut self, _: &mut TransportContext) {}
    fn begin_packet(&mut self, context: &mut TransportContext, header: pes::PesHeader<'_>) {
        match header.contents() {
            pes::PesContents::Parsed(Some(parsed)) => {
                context.audio.extend_from_slice(parsed.payload())
            }
            pes::PesContents::Payload(payload) => context.audio.extend_from_slice(payload),
            pes::PesContents::Parsed(None) => context.failed = true,
        }
    }
    fn continue_packet(&mut self, context: &mut TransportContext, data: &[u8]) {
        context.audio.extend_from_slice(data);
    }
    fn end_packet(&mut self, _: &mut TransportContext) {}
    fn continuity_error(&mut self, context: &mut TransportContext) {
        context.failed = true;
    }
}

struct Transport {
    context: TransportContext,
    demux: demultiplex::Demultiplex<TransportContext>,
}

impl Transport {
    fn new() -> Self {
        let mut context = TransportContext {
            changes: Default::default(),
            audio: Vec::new(),
            selected: None,
            failed: false,
        };
        let demux = demultiplex::Demultiplex::new(&mut context);
        Self { context, demux }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        if !bytes.len().is_multiple_of(188) {
            bail!("Truncated SoundCloud MPEG transport segment");
        }
        for packet in bytes.as_chunks::<188>().0 {
            if packet[0] != 0x47 || packet[1] & 0x80 != 0 {
                bail!("Invalid SoundCloud MPEG transport packet");
            }
            if packet[3] & 0xc0 != 0 {
                bail!("Scrambled SoundCloud audio is not supported");
            }
        }
        self.demux.push(&mut self.context, bytes);
        if self.context.failed {
            bail!("SoundCloud audio packets were lost or malformed. Try playing the song again.");
        }
        if self.context.selected.is_none() {
            bail!("SoundCloud transport contains no supported AAC audio");
        }
        Ok(std::mem::take(&mut self.context.audio))
    }
}

struct SegmentSource {
    rx: mpsc::Receiver<Result<Chunk, String>>,
    current: io::Cursor<Vec<u8>>,
}

impl Read for SegmentSource {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let read = self.current.read(bytes)?;
            if read > 0 {
                return Ok(read);
            }
            match self.rx.blocking_recv() {
                Some(Ok(chunk)) => self.current = io::Cursor::new(chunk.bytes),
                Some(Err(message)) => return Err(io::Error::other(message)),
                None => return Ok(0),
            }
        }
    }
}

impl Seek for SegmentSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "HLS is streamed",
        ))
    }
}

impl MediaSource for SegmentSource {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn decode(
    config: EngineConfig,
    mut rx: mpsc::Receiver<Result<Chunk, String>>,
    position_ms: u32,
    control: &Arc<Control>,
    notify: &Notify,
) -> Result<()> {
    let first = match rx.blocking_recv() {
        Some(Ok(chunk)) => chunk,
        Some(Err(message)) => bail!(message),
        None => return Ok(()),
    };
    let skip_ms = first.skip_ms;
    let source = SegmentSource {
        rx,
        current: io::Cursor::new(first.bytes),
    };
    let source = MediaSourceStream::new(Box::new(source), Default::default());
    let probe = symphonia::default::get_probe().format(
        &Hint::new(),
        source,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probe.format;
    let track = format
        .default_track()
        .context("SoundCloud stream has no audio track")?;
    let track_id = track.id;
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;
    let audio = AudioControl::new(config.buffer_ms);
    let errors: crate::sink::ErrorHook = Arc::new(|_| {});
    let sink = Box::new(RodioSink::new(
        config.audio_device,
        errors,
        Box::new(Volume(Arc::clone(control))),
        config.buffer_ms,
        Arc::clone(&audio),
    ));
    let tapped = Box::new(Tapped::new(
        sink,
        Arc::clone(&audio),
        config.tap,
        Box::new(Volume(Arc::clone(control))),
        false,
        config.eq,
        Arc::new(std::sync::atomic::AtomicU64::new(1.0f64.to_bits())),
    ));
    let mut sink = crate::player::owned_sink(tapped, audio, crate::player::OutputOwner::LocalMix);
    let mut converter = Converter::new(None);
    let mut resampler = None;
    let mut previous_rate = 0;
    let mut skipped = 0u64;
    let mut frames = 0u64;
    let mut started = false;
    let mut decode_failed = false;
    let mut last_report = Instant::now();
    let result = (|| -> Result<()> {
        loop {
            if control.cancelled.load(Ordering::Acquire) {
                break;
            }
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(DecodeError::IoError(error))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            if packet.track_id() != track_id {
                continue;
            }
            let decoded = match decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(DecodeError::DecodeError(_)) => {
                    decode_failed = true;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let spec = *decoded.spec();
            let channels = spec.channels.count();
            if channels == 0 || spec.rate == 0 {
                bail!("SoundCloud stream has an invalid audio format");
            }
            let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
            buffer.copy_interleaved_ref(decoded);
            let mut stereo = Vec::with_capacity(buffer.samples().len() * 2 / channels);
            for frame in buffer.samples().chunks_exact(channels) {
                if channels == 1 {
                    stereo.extend_from_slice(&[frame[0], frame[0]]);
                } else {
                    stereo.extend_from_slice(&frame[..2]);
                }
            }
            if previous_rate != spec.rate {
                resampler = Resampler::new(spec.rate, 44_100, 2);
                previous_rate = spec.rate;
            }
            let stereo = match &mut resampler {
                Some(resampler) => resampler.process(&stereo),
                None => stereo,
            };
            let skip_frames = (u64::from(skip_ms) * 44_100 / 1000)
                .saturating_sub(skipped)
                .min((stereo.len() / 2) as u64);
            skipped += skip_frames;
            for chunk in stereo[skip_frames as usize * 2..].chunks(PCM_CHUNK) {
                if control.cancelled.load(Ordering::Acquire) {
                    return Ok(());
                }
                let mut paused = control.paused.lock().unwrap_or_else(|p| p.into_inner());
                if *paused {
                    if started {
                        sink.stop()
                            .map_err(|error| anyhow!("Audio output failed: {error}"))?;
                    }
                    while *paused && !control.cancelled.load(Ordering::Acquire) {
                        paused = control
                            .changed
                            .wait(paused)
                            .unwrap_or_else(|p| p.into_inner());
                    }
                    if control.cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    sink.start()
                        .map_err(|error| anyhow!("Audio output failed: {error}"))?;
                }
                drop(paused);
                if !started {
                    sink.start()
                        .map_err(|error| anyhow!("Audio output failed: {error}"))?;
                    started = true;
                    notify(StreamEvent::Playing(position_ms));
                }
                sink.write(
                    AudioPacket::Samples(chunk.iter().map(|value| f64::from(*value)).collect()),
                    &mut converter,
                )
                .map_err(|error| anyhow!("Audio output failed: {error}"))?;
                frames += (chunk.len() / 2) as u64;
                if last_report.elapsed() >= Duration::from_millis(250) {
                    notify(StreamEvent::Position(position_ms.saturating_add(
                        (frames * 1000 / 44_100).min(u64::from(u32::MAX)) as u32,
                    )));
                    last_report = Instant::now();
                }
            }
        }
        if !started && decode_failed {
            bail!("This SoundCloud stream could not be decoded. Try again or choose another song.");
        }
        Ok(())
    })();
    let _ = sink.stop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entirely_corrupt_aac_is_an_error_while_a_valid_seek_past_audio_ends_normally() {
        let run = |bytes: Vec<u8>, skip_ms: u32| {
            let (tx, rx) = mpsc::channel(1);
            tx.try_send(Ok(Chunk { bytes, skip_ms })).unwrap();
            drop(tx);
            let config = EngineConfig {
                device_name: "AAC corruption regression".into(),
                bitrate_kbps: 320,
                normalisation: false,
                autoplay: false,
                gapless: false,
                audio_device: None,
                initial_volume: 0,
                volume_dir: Default::default(),
                audio_cache_dir: None,
                audio_cache_limit: None,
                buffer_ms: crate::sink::DEFAULT_BUFFER_MS,
                tap: crate::vis::AudioTap::new(),
                eq: crate::eq::shared(),
                proxy: crate::settings::ProxyConfig::Off,
                local_files: None,
            };
            let control = Arc::new(Control {
                cancelled: AtomicBool::new(false),
                paused: Mutex::new(false),
                changed: Condvar::new(),
                volume: AtomicU16::new(0),
            });
            let notify: Notify =
                Arc::new(|_| panic!("corrupt or fully skipped audio must not start output"));
            decode(config, rx, skip_ms, &control, &notify)
        };
        let valid = [
            0xff, 0xf1, 0x50, 0x80, 0x01, 0xbf, 0xfc, 0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c,
        ];
        let mut corrupt = valid;
        // Set the reserved ICS bit in an otherwise valid AAC-LC frame. The
        // ADTS reader accepts it, then the native decoder rejects its audio.
        corrupt[8] |= 0x80;
        let error = run(corrupt.repeat(6), 0).unwrap_err().to_string();
        assert!(error.contains("could not be decoded"), "{error}");
        assert!(run(valid.repeat(6), 10_000).is_ok());
    }

    #[test]
    fn playlist_resolves_relative_segments_ranges_and_seek_times() {
        let base = Url::parse("https://audio.example/path/list.m3u8?secret=x").unwrap();
        let parsed = parse_playlist(&base, "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"100@20\"\n#EXTINF:5.5,\n#EXT-X-BYTERANGE:50@100\na.m4s\n#EXTINF:4,\n#EXT-X-BYTERANGE:70\na.m4s\n#EXT-X-ENDLIST").unwrap();
        assert_eq!(parsed.segments[0].duration_ms, 5500);
        assert_eq!(parsed.segments[1].range, Some((150, 70)));
        assert_eq!(
            parsed.segments[0].url.as_str(),
            "https://audio.example/path/a.m4s"
        );
        assert_eq!(parsed.map.unwrap().1, Some((20, 100)));
    }

    #[test]
    fn encrypted_hls_and_unsafe_stream_addresses_are_refused() {
        let base = Url::parse("https://audio.example/list").unwrap();
        assert!(
            parse_playlist(
                &base,
                "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n#EXTINF:5,\na.aac"
            )
            .is_err()
        );
        assert!(parse_playlist(&base, "#EXTM3U\n#EXTINF:5,\nhttp://audio.example/a.aac").is_err());
        assert!(
            byte_range(
                &m3u8_rs::ByteRange {
                    length: u64::MAX,
                    offset: Some(20)
                },
                0
            )
            .is_err()
        );
    }

    #[test]
    fn master_chooses_the_highest_audio_rendition_and_tracks_init_changes() {
        let base = Url::parse("https://audio.example/list.m3u8").unwrap();
        let master = parse_playlist(&base, "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=96000,CODECS=\"mp4a.40.2\"\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x480\nvideo.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=160000,CODECS=\"mp4a.40.2\"\nhigh.m3u8\n").unwrap();
        assert_eq!(
            master.variant.unwrap().as_str(),
            "https://audio.example/high.m3u8"
        );
        let media = parse_playlist(&base, "#EXTM3U\n#EXT-X-MAP:URI=\"one.mp4\"\n#EXTINF:5,\none.m4s\n#EXTINF:5,\ntwo.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"two.mp4\"\n#EXTINF:5,\nthree.m4s\n#EXT-X-ENDLIST\n").unwrap();
        assert_eq!(media.segments[0].map, media.segments[1].map);
        assert_ne!(media.segments[1].map, media.segments[2].map);
        assert!(media.segments[2].discontinuity);
    }

    fn ts_packet(pid: u16, counter: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 183);
        let adaptation = 183 - payload.len();
        let mut packet = vec![
            0x47,
            0x40 | ((pid >> 8) as u8),
            pid as u8,
            0x30 | counter,
            adaptation as u8,
            0,
        ];
        packet.resize(5 + adaptation, 0xff);
        packet.extend_from_slice(payload);
        assert_eq!(packet.len(), 188);
        packet
    }

    fn section_packet(pid: u16, mut section: Vec<u8>) -> Vec<u8> {
        let crc = mpeg2ts_reader::mpegts_crc::sum32(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(mpeg2ts_reader::mpegts_crc::sum32(&section), 0);
        section.insert(0, 0); // pointer_field
        ts_packet(pid, 0, &section)
    }

    fn transport_tables() -> Vec<u8> {
        let mut bytes = section_packet(0, vec![0, 0xb0, 0x0d, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0]);
        // H264 PID 0x101 precedes AAC PID 0x102. The demux must select the
        // announced AAC stream rather than the first PES stream encountered.
        bytes.extend(section_packet(
            0x100,
            vec![
                2, 0xb0, 0x17, 0, 1, 0xc1, 0, 0, 0xe1, 2, 0xf0, 0, 0x1b, 0xe1, 1, 0xf0, 0, 0x0f,
                0xe1, 2, 0xf0, 0,
            ],
        ));
        bytes
    }

    fn pes_packet(pid: u16, counter: u8, data: &[u8]) -> Vec<u8> {
        let length = data.len() + 3;
        let mut payload = vec![0, 0, 1, 0xc0, (length >> 8) as u8, length as u8, 0x80, 0, 0];
        payload.extend_from_slice(data);
        ts_packet(pid, counter, &payload)
    }

    #[test]
    fn transport_selects_announced_aac_and_keeps_continuity_between_segments() {
        let mut bytes = transport_tables();
        bytes.extend(pes_packet(0x101, 0, b"not audio"));
        bytes.extend(pes_packet(0x102, 0, b"first"));
        let mut transport = Transport::new();
        assert_eq!(transport.push(&bytes).unwrap(), b"first");
        assert_eq!(
            transport.push(&pes_packet(0x102, 1, b"second")).unwrap(),
            b"second"
        );
        assert!(
            transport
                .push(&pes_packet(0x102, 3, b"lost packet"))
                .is_err()
        );
    }

    #[test]
    fn transport_refuses_scrambling_and_audio_without_program_tables() {
        assert!(
            Transport::new()
                .push(&pes_packet(0x102, 0, b"audio"))
                .is_err()
        );
        let mut bytes = transport_tables();
        let mut scrambled = pes_packet(0x102, 0, b"audio");
        scrambled[3] |= 0x80;
        bytes.extend(scrambled);
        assert!(Transport::new().push(&bytes).is_err());
    }

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut result = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        result.extend_from_slice(kind);
        result.extend_from_slice(body);
        result
    }

    #[test]
    fn fragmented_mp4_frames_become_length_checked_adts() {
        let mut trun = vec![0, 0, 2, 0];
        trun.extend_from_slice(&2u32.to_be_bytes());
        trun.extend_from_slice(&3u32.to_be_bytes());
        trun.extend_from_slice(&2u32.to_be_bytes());
        let moof = atom(b"moof", &atom(b"traf", &atom(b"trun", &trun)));
        let mut bytes = moof;
        bytes.extend(atom(b"mdat", &[1, 2, 3, 4, 5]));
        let audio = fragmented_aac(
            &bytes,
            AacConfig {
                profile: 1,
                frequency: 4,
                channels: 2,
                default_size: 0,
            },
        )
        .unwrap();
        assert_eq!(&audio[7..10], &[1, 2, 3]);
        assert_eq!(&audio[17..], &[4, 5]);
        assert_eq!(audio.len(), 19);
        bytes.pop();
        assert!(
            fragmented_aac(
                &bytes,
                AacConfig {
                    profile: 1,
                    frequency: 4,
                    channels: 2,
                    default_size: 0
                }
            )
            .is_err()
        );
    }

    #[test]
    fn native_aac_decodes_silence_across_transport_chunk_boundaries() {
        // One AAC-LC stereo silence block, framed as ADTS at 44.1 kHz.
        // Keeping this tiny synthesized fixture inline avoids a network or
        // audio-device dependency while exercising the production decoder.
        let frame = [
            0xff, 0xf1, 0x50, 0x80, 0x01, 0xbf, 0xfc, 0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c,
        ];
        let bytes = frame.repeat(6);
        let (tx, rx) = mpsc::channel(2);
        tx.try_send(Ok(Chunk {
            bytes: bytes[..9].to_vec(),
            skip_ms: 0,
        }))
        .unwrap();
        tx.try_send(Ok(Chunk {
            bytes: bytes[9..].to_vec(),
            skip_ms: 0,
        }))
        .unwrap();
        drop(tx);
        let source = SegmentSource {
            rx,
            current: io::Cursor::new(Vec::new()),
        };
        let source = MediaSourceStream::new(Box::new(source), Default::default());
        let mut format = symphonia::default::get_probe()
            .format(
                &Hint::new(),
                source,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .unwrap()
            .format;
        let track = format.default_track().unwrap();
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .unwrap();
        let mut frames = 0;
        while let Ok(packet) = format.next_packet() {
            let decoded = decoder.decode(&packet).unwrap();
            assert_eq!(decoded.spec().rate, 44_100);
            assert_eq!(decoded.spec().channels.count(), 2);
            let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            frames += decoded.frames();
            samples.copy_interleaved_ref(decoded);
            assert!(samples.samples().iter().all(|sample| sample.abs() < 0.0001));
        }
        assert!(frames >= 5 * 1024);
    }
}
