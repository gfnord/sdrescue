//! Rebuilding AVI videos whose header was overwritten.
//!
//! An AVI file is `RIFF 'AVI '` + `LIST hdrl` (the stream formats: codec,
//! frame size, frame rate, audio format) + `LIST movi` (the actual data) + an
//! optional `idx1` index. Inside `movi` the data is a plain sequence of chunks,
//! each an 8-byte header (`id`, `u32 LE size`) followed by the payload (padded
//! to an even length):
//!
//! - `NNdc` / `NNdb`: a video frame of stream `NN` (with MJPEG, a complete JPEG),
//! - `NNwb`: audio of stream `NN`,
//! - `ixNN` (OpenDML index), `JUNK` (padding), `LIST rec ` (grouping), and in
//!   files over 1 GB a `RIFF AVIX` + `LIST movi` continuation.
//!
//! Cameras write the header first, so when a card is reformatted and new photos
//! are taken, the header (in the first clusters) is the first thing to be
//! overwritten while the frames behind it survive. Such a stream has no
//! signature at a cluster start, so the normal carvers cannot see it.
//!
//! Pass 1 of the engine records *hints* (positions anywhere, not only aligned,
//! where two consecutive chunk headers look valid). Here a hint is walked chunk
//! by chunk to the end of the stream, and a new AVI is written around the
//! surviving chunks: either with a header copied from a reference video made by
//! the same camera, or with one inferred from the data (MJPEG frame size from
//! the first frame's SOF marker, an assumed frame rate, and 16-bit PCM audio
//! whose sample rate is deduced from the audio/video byte ratio).

use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::Input;

/// Largest chunk accepted (a 4K MJPEG frame is a few MB).
const MAX_CHUNK: u32 = 32 << 20;
/// A stream needs at least this many video frames to be rebuilt.
pub const MIN_VIDEO_FRAMES: usize = 16;
/// Hints are kept at most once per this many bytes (a stream yields one hint
/// per MiB instead of one per frame).
pub const HINT_SPACING: u64 = 1 << 20;
/// Largest `movi` payload written into one rebuilt file (AVI sizes are 32-bit).
pub const MAX_PART_BYTES: u64 = (4 << 30) - (64 << 20);

/// What a chunk id means inside `movi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkKind {
    /// `NNdc` / `NNdb`.
    Video(u8),
    /// `NNwb`.
    Audio(u8),
    /// `ixNN`, `JUNK`, `NNpc`, `NNtx`: skipped.
    Skip,
    /// `LIST` (`rec `/`movi`) or `RIFF` (`AVIX`): step into its content.
    Enter,
    /// `idx1`: end of the data.
    End,
}

fn kind(id: &[u8]) -> Option<ChunkKind> {
    if id.len() < 4 {
        return None;
    }
    let digits = id[0].is_ascii_digit() && id[1].is_ascii_digit();
    let n = if digits {
        (id[0] - b'0') * 10 + (id[1] - b'0')
    } else {
        0
    };
    match (&id[..2], &id[2..4]) {
        _ if digits && (&id[2..4] == b"dc" || &id[2..4] == b"db") => Some(ChunkKind::Video(n)),
        _ if digits && &id[2..4] == b"wb" => Some(ChunkKind::Audio(n)),
        _ if digits && (&id[2..4] == b"pc" || &id[2..4] == b"tx") => Some(ChunkKind::Skip),
        (b"ix", d) if d[0].is_ascii_digit() && d[1].is_ascii_digit() => Some(ChunkKind::Skip),
        _ if id[..4] == *b"JUNK" => Some(ChunkKind::Skip),
        _ if id[..4] == *b"LIST" || id[..4] == *b"RIFF" => Some(ChunkKind::Enter),
        _ if id[..4] == *b"idx1" => Some(ChunkKind::End),
        _ => None,
    }
}

/// Chunk ids that can start a stream hint (the engine searches for these).
pub fn hint_patterns() -> Vec<[u8; 4]> {
    let mut v = Vec::new();
    for n in 0..4u8 {
        for suffix in [b"dc", b"db", b"wb"] {
            v.push([b'0', b'0' + n, suffix[0], suffix[1]]);
        }
    }
    v
}

fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Whether a data chunk header at `buf[p..]` is followed by another valid
/// chunk header (or runs past the end of `buf`). Used by the pass-1 scan.
pub fn is_hint(buf: &[u8], p: usize) -> bool {
    let Some(size) = le32(buf, p + 4) else {
        return false;
    };
    if size == 0 || size > MAX_CHUNK {
        return false;
    }
    if !matches!(
        kind(&buf[p..p + 4]),
        Some(ChunkKind::Video(_) | ChunkKind::Audio(_))
    ) {
        return false;
    }
    let next = p + 8 + size as usize + (size as usize & 1);
    match buf.get(next..next + 4) {
        Some(id) => kind(id).is_some(),
        None => true,
    }
}

/// One data chunk of a stream (header position, id, payload size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamChunk {
    /// Absolute offset of the 8-byte chunk header.
    pub offset: u64,
    /// Chunk id, e.g. `00dc`.
    pub id: [u8; 4],
    /// Payload size in bytes.
    pub size: u32,
}

impl StreamChunk {
    fn kind(&self) -> Option<ChunkKind> {
        kind(&self.id)
    }
}

/// A run of AVI `movi` chunks found without its header.
#[derive(Debug, Clone)]
pub struct OrphanStream {
    /// Absolute offset of the first chunk header.
    pub start: u64,
    /// Absolute offset just after the last chunk.
    pub end: u64,
    /// The video and audio chunks, in order.
    pub chunks: Vec<StreamChunk>,
}

impl OrphanStream {
    /// Number of video frames.
    pub fn video_frames(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| matches!(c.kind(), Some(ChunkKind::Video(_))))
            .count()
    }

    /// Stream numbers used, sorted, with whether each is video.
    fn streams(&self) -> Vec<(u8, bool)> {
        let mut v: Vec<(u8, bool)> = Vec::new();
        for c in &self.chunks {
            let (n, video) = match c.kind() {
                Some(ChunkKind::Video(n)) => (n, true),
                Some(ChunkKind::Audio(n)) => (n, false),
                _ => continue,
            };
            if !v.iter().any(|(m, _)| *m == n) {
                v.push((n, video));
            }
        }
        v.sort();
        v
    }
}

/// Walks chunks from `start` while they stay valid and before `limit`.
/// Returns `None` if fewer than [`MIN_VIDEO_FRAMES`] video frames are found.
pub fn walk(input: &Input, start: u64, limit: u64) -> Option<OrphanStream> {
    let mut pos = start;
    let mut chunks = Vec::new();
    let mut end = start;
    while pos + 8 <= limit {
        let Some(hdr) = input.array::<8>(pos) else {
            break;
        };
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let Some(k) = kind(&hdr[..4]) else { break };
        match k {
            ChunkKind::End => break,
            ChunkKind::Enter => {
                // LIST/RIFF header: size, then a 4-byte list type; the content follows.
                pos += 12;
                continue;
            }
            _ => {}
        }
        if size > MAX_CHUNK {
            break;
        }
        let next = pos + 8 + u64::from(size) + u64::from(size & 1);
        if next > limit {
            break;
        }
        if matches!(k, ChunkKind::Video(_) | ChunkKind::Audio(_)) {
            let mut id = [0u8; 4];
            id.copy_from_slice(&hdr[..4]);
            chunks.push(StreamChunk {
                offset: pos,
                id,
                size,
            });
        }
        end = next;
        pos = next;
    }
    let s = OrphanStream { start, end, chunks };
    (s.video_frames() >= MIN_VIDEO_FRAMES).then_some(s)
}

/// A stream header list (`LIST hdrl`) to put in front of the rebuilt data,
/// plus a human-readable description for the report.
#[derive(Debug, Clone)]
pub struct Template {
    /// The complete `LIST hdrl` chunk.
    pub hdrl: Vec<u8>,
    /// E.g. `MJPEG 1920x1080, 30 fps (assumed), PCM 16000 Hz mono (inferred)`.
    pub description: String,
}

fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(data.len() + 9);
    v.extend_from_slice(id);
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    if data.len() & 1 == 1 {
        v.push(0);
    }
    v
}

fn list(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let mut d = kind.to_vec();
    d.extend_from_slice(content);
    chunk(b"LIST", &d)
}

fn u32s(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// `strh` for a stream: type, handler, scale/rate, sample size, frame rectangle.
fn strh(
    fcc_type: &[u8; 4],
    handler: &[u8; 4],
    scale: u32,
    rate: u32,
    sample_size: u32,
    w: u16,
    h: u16,
) -> Vec<u8> {
    let mut d = Vec::with_capacity(56);
    d.extend_from_slice(fcc_type);
    d.extend_from_slice(handler);
    d.extend(u32s(&[0])); // flags
    d.extend_from_slice(&[0, 0, 0, 0]); // priority, language
    d.extend(u32s(&[
        0,
        scale,
        rate,
        0,
        0,
        1 << 20,
        u32::MAX,
        sample_size,
    ]));
    d.extend_from_slice(&[0, 0, 0, 0]);
    d.extend_from_slice(&w.to_le_bytes());
    d.extend_from_slice(&h.to_le_bytes());
    chunk(b"strh", &d)
}

/// Frame size from the SOFn marker of a JPEG (searches the first 64 KiB).
fn jpeg_size(data: &[u8]) -> Option<(u16, u16)> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut p = 2usize;
    while p + 9 < data.len() {
        if data[p] != 0xFF {
            return None;
        }
        let m = data[p + 1];
        if m == 0xFF {
            p += 1;
            continue;
        }
        let len = u16::from_be_bytes([data[p + 2], data[p + 3]]) as usize;
        if matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let h = u16::from_be_bytes([data[p + 5], data[p + 6]]);
            let w = u16::from_be_bytes([data[p + 7], data[p + 8]]);
            return (w > 0 && h > 0).then_some((w, h));
        }
        if m == 0xDA || len < 2 {
            return None;
        }
        p += 2 + len;
    }
    None
}

/// Standard PCM sample rates, tried when deducing the audio format.
const RATES: [u32; 9] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];

/// Builds a header from the data alone. Only MJPEG video (stream 00) with
/// optional PCM audio (stream 01) can be inferred.
pub fn infer_template(input: &Input, s: &OrphanStream, fps: u32) -> Result<Template, String> {
    let streams = s.streams();
    let layout_ok = matches!(streams.as_slice(), [(0, true)] | [(0, true), (1, false)]);
    if !layout_ok {
        return Err(format!(
            "unusual stream layout {:?}; pass --avi-reference <a video from the same camera>",
            streams
                .iter()
                .map(|(n, v)| format!("{n:02}{}", if *v { "dc" } else { "wb" }))
                .collect::<Vec<_>>()
        ));
    }
    let mut size = None;
    for c in s
        .chunks
        .iter()
        .filter(|c| matches!(c.kind(), Some(ChunkKind::Video(_))))
        .take(32)
    {
        let n = (c.size as usize).min(64 * 1024);
        if let Some(d) = input.bytes(c.offset + 8, n)
            && let Some(wh) = jpeg_size(&d)
        {
            size = Some(wh);
            break;
        }
    }
    let Some((w, h)) = size else {
        return Err(
            "the video is not MJPEG, so its format cannot be guessed; pass --avi-reference <a video from the same camera>"
                .into(),
        );
    };
    let frames = s.video_frames() as u32;
    let fps = fps.max(1);
    let mut strls = list(
        b"strl",
        &[
            strh(b"vids", b"MJPG", 1, fps, 0, w, h),
            chunk(
                b"strf",
                &[
                    u32s(&[40, u32::from(w), u32::from(h)]),
                    vec![1, 0, 24, 0],
                    b"MJPG".to_vec(),
                    u32s(&[u32::from(w) * u32::from(h) * 3, 0, 0, 0, 0]),
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    let mut desc = format!("MJPEG {w}x{h}, {fps} fps (assumed)");
    let audio_bytes: u64 = s
        .chunks
        .iter()
        .filter(|c| matches!(c.kind(), Some(ChunkKind::Audio(_))))
        .map(|c| u64::from(c.size))
        .sum();
    if audio_bytes > 0 {
        // Bytes per second of audio, given the assumed frame rate.
        let bps = audio_bytes as f64 * f64::from(fps) / f64::from(frames.max(1));
        // Mono is tried first: on a tie (16 kHz mono = 8 kHz stereo) mono wins,
        // as camera microphones are almost always mono.
        let (rate, channels) = RATES
            .iter()
            .map(|&r| (r, 1u16))
            .chain(RATES.iter().map(|&r| (r, 2u16)))
            .min_by(|a, b| {
                let da = (f64::from(a.0 * u32::from(a.1) * 2) - bps).abs();
                let db = (f64::from(b.0 * u32::from(b.1) * 2) - bps).abs();
                da.total_cmp(&db)
            })
            .unwrap_or((16000, 1));
        let align = channels * 2;
        let byte_rate = rate * u32::from(align);
        strls.extend(list(
            b"strl",
            &[
                strh(
                    b"auds",
                    &[0; 4],
                    u32::from(align),
                    byte_rate,
                    u32::from(align),
                    0,
                    0,
                ),
                chunk(
                    b"strf",
                    &[
                        vec![1, 0],
                        channels.to_le_bytes().to_vec(),
                        u32s(&[rate, byte_rate]),
                        align.to_le_bytes().to_vec(),
                        vec![16, 0, 0, 0],
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ));
        desc.push_str(&format!(
            ", PCM {rate} Hz {} (inferred)",
            if channels == 1 { "mono" } else { "stereo" }
        ));
    }
    let nstreams = if audio_bytes > 0 { 2 } else { 1 };
    let avih = chunk(
        b"avih",
        &u32s(&[
            1_000_000 / fps,
            0,
            0,
            0x10,
            frames,
            0,
            nstreams,
            1 << 20,
            u32::from(w),
            u32::from(h),
            0,
            0,
            0,
            0,
        ]),
    );
    let hdrl = list(b"hdrl", &[avih, strls].concat());
    Ok(Template {
        hdrl,
        description: desc,
    })
}

/// Takes the `LIST hdrl` of a reference AVI recorded by the same camera.
pub fn template_from_reference(path: &Path) -> Result<Template> {
    let data = {
        use std::io::Read;
        let mut f = std::fs::File::open(path)
            .with_context(|| format!("cannot open reference video {}", path.display()))?;
        let mut v = Vec::new();
        Read::take(&mut f, 4 << 20).read_to_end(&mut v)?;
        v
    };
    if data.len() < 24 || &data[0..4] != b"RIFF" || &data[8..12] != b"AVI " {
        bail!("{} is not an AVI file", path.display());
    }
    let mut p = 12usize;
    while p + 12 <= data.len() {
        let id = &data[p..p + 4];
        let size = le32(&data, p + 4).unwrap_or(0) as usize;
        if id == b"LIST" && &data[p + 8..p + 12] == b"hdrl" {
            let end = p + 8 + size;
            let hdrl = data
                .get(p..end)
                .with_context(|| format!("the header of {} is incomplete", path.display()))?
                .to_vec();
            return Ok(Template {
                hdrl,
                description: format!("header from {}", path.display()),
            });
        }
        p += 8 + size + (size & 1);
    }
    bail!("no stream header (hdrl) found in {}", path.display())
}

/// Walks the sub-chunks of a LIST payload `[start, end)`, calling `f(id, data_start, size)`.
fn for_each_chunk(buf: &[u8], start: usize, end: usize, f: &mut dyn FnMut(&[u8], usize, usize)) {
    let mut p = start;
    while p + 8 <= end.min(buf.len()) {
        let size = le32(buf, p + 4).unwrap_or(0) as usize;
        let data = p + 8;
        if data + size > buf.len() {
            break;
        }
        f(&buf[p..p + 4], data, size);
        p = data + size + (size & 1);
    }
}

fn put32(buf: &mut [u8], at: usize, v: u32) {
    if let Some(s) = buf.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// Adapts a template to the data being written: total frames, per-stream
/// lengths, and `indx` super-indexes (which point into the original file)
/// turned into `JUNK`. Fails if the template's streams don't match the data.
fn patch_hdrl(t: &Template, chunks: &[StreamChunk]) -> Result<Vec<u8>, String> {
    let mut h = t.hdrl.clone();
    if h.len() < 12 || &h[0..4] != b"LIST" || &h[8..12] != b"hdrl" {
        return Err("invalid stream header".into());
    }
    let frames = chunks
        .iter()
        .filter(|c| matches!(c.kind(), Some(ChunkKind::Video(_))))
        .count() as u32;
    let mut avih_at = None;
    let mut dmlh_at = None;
    let mut strls: Vec<(usize, usize)> = Vec::new();
    let len = h.len();
    for_each_chunk(&h.clone(), 12, len, &mut |id, d, size| match id {
        b"avih" => avih_at = Some(d),
        b"LIST" if size >= 4 && h.get(d..d + 4) == Some(b"strl") => strls.push((d + 4, d + size)),
        b"LIST" if size >= 4 && h.get(d..d + 4) == Some(b"odml") => {
            for_each_chunk(&h.clone(), d + 4, d + size, &mut |id2, d2, _| {
                if id2 == b"dmlh" {
                    dmlh_at = Some(d2);
                }
            });
        }
        _ => {}
    });
    let avih = avih_at.ok_or("no avih in the stream header")?;
    put32(&mut h, avih + 16, frames);
    if let Some(d) = dmlh_at {
        put32(&mut h, d, frames);
    }
    let snapshot = h.clone();
    for (n, &(s, e)) in strls.iter().enumerate() {
        let mut strh_at = None;
        let mut indx_at = Vec::new();
        for_each_chunk(&snapshot, s, e, &mut |id, d, _| match id {
            b"strh" => strh_at = Some(d),
            b"indx" => indx_at.push(d - 8),
            _ => {}
        });
        for at in indx_at {
            h[at..at + 4].copy_from_slice(b"JUNK");
        }
        let Some(sh) = strh_at else { continue };
        let fcc = snapshot.get(sh..sh + 4).unwrap_or(&[]);
        let mine: Vec<&StreamChunk> = chunks
            .iter()
            .filter(|c| match c.kind() {
                Some(ChunkKind::Video(m) | ChunkKind::Audio(m)) => usize::from(m) == n,
                _ => false,
            })
            .collect();
        if let Some(c) = mine.first() {
            let video = matches!(c.kind(), Some(ChunkKind::Video(_)));
            if (video && fcc != b"vids") || (!video && fcc != b"auds") {
                return Err(format!(
                    "stream {n:02} of the data is {} but the reference header says {}",
                    if video { "video" } else { "audio" },
                    String::from_utf8_lossy(fcc)
                ));
            }
        }
        let sample_size = le32(&snapshot, sh + 44).unwrap_or(0);
        let length = if sample_size > 0 {
            mine.iter().map(|c| u64::from(c.size)).sum::<u64>() / u64::from(sample_size)
        } else {
            mine.len() as u64
        };
        put32(&mut h, sh + 32, length.min(u64::from(u32::MAX)) as u32);
    }
    let max_stream = chunks
        .iter()
        .filter_map(|c| match c.kind() {
            Some(ChunkKind::Video(m) | ChunkKind::Audio(m)) => Some(usize::from(m)),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    if max_stream >= strls.len() {
        return Err(format!(
            "the data has stream {max_stream:02} but the header describes only {} stream(s)",
            strls.len()
        ));
    }
    Ok(h)
}

/// First stream hint in `[from, to)` (at most [`HINT_SPACING`] bytes are
/// searched). Used after a stream ends, to find another stream starting in
/// the same MiB, which pass 1 does not report separately.
pub fn find_hint(input: &Input, from: u64, to: u64) -> Option<u64> {
    let to = to.min(from.saturating_add(HINT_SPACING));
    if to <= from {
        return None;
    }
    let buf = {
        let mut b = vec![0u8; (to - from) as usize + 8];
        let n = input.read(from, &mut b);
        b.truncate(n);
        b
    };
    let ac = aho_corasick::AhoCorasick::new(hint_patterns()).ok()?;
    let span = ((to - from) as usize).min(buf.len());
    ac.find_iter(&buf[..span])
        .map(|m| m.start())
        .find(|&p| is_hint(&buf, p))
        .map(|p| from + p as u64)
}

/// Splits a stream's chunks into parts whose data fits in one AVI file.
pub fn split_parts(chunks: &[StreamChunk], max_bytes: u64) -> Vec<&[StreamChunk]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut bytes = 0u64;
    for (i, c) in chunks.iter().enumerate() {
        let sz = 8 + u64::from(c.size) + u64::from(c.size & 1);
        if bytes + sz > max_bytes && i > start {
            parts.push(&chunks[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += sz;
    }
    if start < chunks.len() {
        parts.push(&chunks[start..]);
    }
    parts
}

/// Writes a complete AVI (header from `t`, the given chunks copied from the
/// source, and an `idx1` index). Returns the number of bytes written.
pub fn write_avi(
    input: &Input,
    chunks: &[StreamChunk],
    t: &Template,
    out: &mut impl Write,
) -> Result<u64> {
    let hdrl = patch_hdrl(t, chunks).map_err(|e| anyhow::anyhow!(e))?;
    let movi_data: u64 = chunks
        .iter()
        .map(|c| 8 + u64::from(c.size) + u64::from(c.size & 1))
        .sum();
    let movi_size = 4 + movi_data;
    let idx_size = 16 * chunks.len() as u64;
    let riff_size = 4 + hdrl.len() as u64 + 8 + movi_size + 8 + idx_size;
    if riff_size > u64::from(u32::MAX) {
        bail!("rebuilt AVI would exceed 4 GB; split the stream first");
    }
    let mut written = 0u64;
    let mut put = |w: &mut dyn Write, b: &[u8]| -> io::Result<()> {
        w.write_all(b)?;
        written += b.len() as u64;
        Ok(())
    };
    put(out, b"RIFF")?;
    put(out, &(riff_size as u32).to_le_bytes())?;
    put(out, b"AVI ")?;
    put(out, &hdrl)?;
    put(out, b"LIST")?;
    put(out, &(movi_size as u32).to_le_bytes())?;
    put(out, b"movi")?;
    let mut buf = vec![0u8; 1 << 20];
    let mut index = Vec::with_capacity(idx_size as usize);
    let mut rel = 4u32; // offsets are relative to the 'movi' fourcc
    for c in chunks {
        let video = matches!(c.kind(), Some(ChunkKind::Video(_)));
        index.extend_from_slice(&c.id);
        index.extend_from_slice(&(if video { 0x10u32 } else { 0 }).to_le_bytes());
        index.extend_from_slice(&rel.to_le_bytes());
        index.extend_from_slice(&c.size.to_le_bytes());
        put(out, &c.id)?;
        put(out, &c.size.to_le_bytes())?;
        let mut left = u64::from(c.size);
        let mut pos = c.offset + 8;
        while left > 0 {
            let n = (left as usize).min(buf.len());
            let got = input.read(pos, &mut buf[..n]);
            if got < n {
                // The source ended or failed: keep the structure valid with zeros.
                buf[got..n].fill(0);
            }
            put(out, &buf[..n])?;
            left -= n as u64;
            pos += n as u64;
        }
        if c.size & 1 == 1 {
            put(out, &[0])?;
        }
        rel += 8 + c.size + (c.size & 1);
    }
    put(out, b"idx1")?;
    put(out, &(idx_size as u32).to_le_bytes())?;
    put(out, &index)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    /// A tiny JPEG-like frame with an SOF0 marker for `w` x `h`.
    fn frame(w: u16, h: u16, fill: u8, len: usize) -> Vec<u8> {
        let mut f = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 8];
        f.extend(h.to_be_bytes());
        f.extend(w.to_be_bytes());
        f.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        f.resize(len - 2, fill);
        f.extend([0xFF, 0xD9]);
        f
    }

    /// Garbage, then a headerless movi stream: frames, audio, ix00 and JUNK.
    fn orphan(frames: usize, audio_per_frame: usize) -> (Vec<u8>, usize, Vec<Vec<u8>>) {
        let mut v: Vec<u8> = (0..777u32).map(|i| (i * 37 % 251) as u8).collect();
        let start = v.len();
        let mut payloads = Vec::new();
        for i in 0..frames {
            let f = frame(320, 240, i as u8, 1001 + i % 3);
            v.extend(chunk(b"00dc", &f));
            payloads.push(f);
            let a = vec![(i * 3) as u8; audio_per_frame];
            v.extend(chunk(b"01wb", &a));
            payloads.push(a);
            if i == 10 {
                v.extend(chunk(b"ix00", &[0u8; 64]));
                v.extend(chunk(b"JUNK", &[0u8; 30]));
            }
        }
        v.extend(b"\x01\x02\x03\x04 trailing garbage");
        (v, start, payloads)
    }

    #[test]
    fn hint_needs_two_valid_chunks() {
        let (v, start, _) = orphan(20, 100);
        assert!(is_hint(&v, start));
        assert!(!is_hint(&v, start + 1));
        let mut bad = v.clone();
        bad[start + 4..start + 8].copy_from_slice(&5u32.to_le_bytes());
        assert!(!is_hint(&bad, start));
        assert_eq!(hint_patterns().len(), 12);
    }

    #[test]
    fn walks_stream_skipping_index_and_padding() {
        let (v, start, payloads) = orphan(20, 100);
        let src = MemSource(v);
        let inp = Input::new(&src);
        let s = walk(&inp, start as u64, src.0.len() as u64).unwrap();
        assert_eq!(s.video_frames(), 20);
        assert_eq!(s.chunks.len(), payloads.len());
        assert_eq!(s.start, start as u64);
        assert!(src.0[s.end as usize..].starts_with(b"\x01\x02\x03\x04"));
        // Too few frames is not a stream.
        let (v, start, _) = orphan(MIN_VIDEO_FRAMES - 1, 10);
        let src = MemSource(v);
        assert!(walk(&Input::new(&src), start as u64, src.0.len() as u64).is_none());
    }

    #[test]
    fn limit_stops_the_walk() {
        let (v, start, _) = orphan(40, 100);
        let src = MemSource(v);
        let inp = Input::new(&src);
        let full = walk(&inp, start as u64, src.0.len() as u64).unwrap();
        let half = walk(&inp, start as u64, (full.start + full.end) / 2).unwrap();
        assert!(half.video_frames() < 40 && half.end <= (full.start + full.end) / 2);
    }

    fn parse_avi(avi: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>, usize) {
        assert_eq!(&avi[0..4], b"RIFF");
        assert_eq!(le32(avi, 4).unwrap() as usize, avi.len() - 8);
        assert_eq!(&avi[8..12], b"AVI ");
        let hsize = le32(avi, 16).unwrap() as usize;
        let hdrl = avi[12..20 + hsize].to_vec();
        let mut p = 20 + hsize;
        assert_eq!(&avi[p..p + 4], b"LIST");
        let msize = le32(avi, p + 4).unwrap() as usize;
        assert_eq!(&avi[p + 8..p + 12], b"movi");
        let movi_end = p + 8 + msize;
        p += 12;
        let mut payloads = Vec::new();
        while p < movi_end {
            let s = le32(avi, p + 4).unwrap() as usize;
            payloads.push(avi[p + 8..p + 8 + s].to_vec());
            p += 8 + s + (s & 1);
        }
        assert_eq!(&avi[p..p + 4], b"idx1");
        let entries = le32(avi, p + 4).unwrap() as usize / 16;
        (hdrl, payloads, entries)
    }

    #[test]
    fn infers_mjpeg_and_pcm_and_rebuilds() {
        // 30 fps and 1066-1067 audio bytes per frame ~ 32000 B/s = 16 kHz mono.
        let (v, start, payloads) = orphan(30, 1067);
        let src = MemSource(v);
        let inp = Input::new(&src);
        let s = walk(&inp, start as u64, src.0.len() as u64).unwrap();
        let t = infer_template(&inp, &s, 30).unwrap();
        assert_eq!(
            t.description,
            "MJPEG 320x240, 30 fps (assumed), PCM 16000 Hz mono (inferred)"
        );
        let mut out = Vec::new();
        let n = write_avi(&inp, &s.chunks, &t, &mut out).unwrap();
        assert_eq!(n as usize, out.len());
        let (hdrl, got, entries) = parse_avi(&out);
        assert_eq!(got, payloads);
        assert_eq!(entries, payloads.len());
        // avih total frames and the video strh length were patched.
        let avih = hdrl.windows(4).position(|w| w == b"avih").unwrap() + 8;
        assert_eq!(le32(&hdrl, avih + 16), Some(30));
        // The rebuilt file is itself recognised by the RIFF carver.
        let carved =
            crate::carve::testutil::carve_bytes(&crate::carve::riff::Riff, &out, 100).unwrap();
        assert_eq!(carved.len as usize, out.len());
        assert_eq!(carved.ext, "avi");
    }

    #[test]
    fn non_mjpeg_needs_a_reference() {
        let mut v = Vec::new();
        for i in 0..20u8 {
            v.extend(chunk(b"00dc", &[i; 500]));
        }
        let src = MemSource(v);
        let inp = Input::new(&src);
        let s = walk(&inp, 0, src.0.len() as u64).unwrap();
        let e = infer_template(&inp, &s, 30).unwrap_err();
        assert!(e.contains("--avi-reference"), "{e}");
    }

    #[test]
    fn reference_header_is_reused_and_patched() {
        // A "reference" recorded by the same camera: rebuild one, add an indx
        // chunk to its video strl as OpenDML files have, and save it.
        let (v, start, _) = orphan(20, 1067);
        let src = MemSource(v);
        let inp = Input::new(&src);
        let s = walk(&inp, start as u64, src.0.len() as u64).unwrap();
        let mut t = infer_template(&inp, &s, 25).unwrap();
        let strl = t.hdrl.windows(4).position(|w| w == b"strl").unwrap();
        let strl_size_at = strl - 8 + 4;
        let indx = chunk(b"indx", &[7u8; 24]);
        let insert_at = strl + 4 + 64 + 48; // after strh (8+56) and strf (8+40)
        t.hdrl.splice(insert_at..insert_at, indx.iter().copied());
        for at in [4usize, strl_size_at] {
            let v = le32(&t.hdrl, at).unwrap() + indx.len() as u32;
            put32(&mut t.hdrl, at, v);
        }
        // A reference file as written by the camera: header + (empty) movi.
        let movi = list(b"movi", &[]);
        let body = [b"AVI ".to_vec(), t.hdrl.clone(), movi].concat();
        let reference = [
            b"RIFF".to_vec(),
            (body.len() as u32).to_le_bytes().to_vec(),
            body,
        ]
        .concat();
        let dir = std::env::temp_dir().join(format!("sdrescue-avi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ref.avi");
        std::fs::write(&path, &reference).unwrap();

        let rt = template_from_reference(&path).unwrap();
        assert!(rt.hdrl.windows(4).any(|w| w == b"indx"));
        let (v2, start2, payloads2) = orphan(35, 1067);
        let src2 = MemSource(v2);
        let inp2 = Input::new(&src2);
        let s2 = walk(&inp2, start2 as u64, src2.0.len() as u64).unwrap();
        let mut out = Vec::new();
        write_avi(&inp2, &s2.chunks, &rt, &mut out).unwrap();
        let (hdrl, got, _) = parse_avi(&out);
        assert_eq!(got, payloads2);
        assert!(
            !hdrl.windows(4).any(|w| w == b"indx"),
            "indx must be neutralised"
        );
        let avih = hdrl.windows(4).position(|w| w == b"avih").unwrap() + 8;
        assert_eq!(le32(&hdrl, avih + 16), Some(35));
        // 25 fps from the reference is kept.
        assert_eq!(le32(&hdrl, avih), Some(40_000));
        assert!(template_from_reference(&dir.join("missing.avi")).is_err());
        std::fs::write(dir.join("not.avi"), b"RIFF\0\0\0\0WAVEfmt ").unwrap();
        assert!(template_from_reference(&dir.join("not.avi")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reference_with_wrong_streams_is_rejected() {
        let (v, start, _) = orphan(20, 0);
        let src = MemSource(v);
        let inp = Input::new(&src);
        // Video-only header, but data with audio at stream 01.
        let video_only = walk(&inp, start as u64, src.0.len() as u64).unwrap();
        let only: Vec<StreamChunk> = video_only
            .chunks
            .iter()
            .copied()
            .filter(|c| &c.id == b"00dc")
            .collect();
        let t = infer_template(
            &inp,
            &OrphanStream {
                chunks: only,
                ..video_only.clone()
            },
            30,
        )
        .unwrap();
        let (v2, start2, _) = orphan(20, 50);
        let src2 = MemSource(v2);
        let inp2 = Input::new(&src2);
        let s2 = walk(&inp2, start2 as u64, src2.0.len() as u64).unwrap();
        let err = write_avi(&inp2, &s2.chunks, &t, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("stream 01"), "{err}");
    }

    #[test]
    fn splits_large_streams() {
        let chunks: Vec<StreamChunk> = (0..10)
            .map(|i| StreamChunk {
                offset: i * 1000,
                id: *b"00dc",
                size: 992,
            })
            .collect();
        let parts = split_parts(&chunks, 3000);
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![3, 3, 3, 1]
        );
    }
}
