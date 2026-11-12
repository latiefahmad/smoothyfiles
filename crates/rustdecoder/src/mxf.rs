//! MXF (SMPTE ST 377-1): OP1a and OP-Atom files as cameras, recorders and editors write them.
//!
//! [`parse`] scans the file as one run of KLV triplets (16-byte key, BER length, value): the
//! partition packs, the primer pack, the header metadata sets, the index table segments and the
//! essence elements. Only the keys and lengths of essence elements are read, never their
//! payload, so a file of any size is indexed by reading a few bytes per element. From the
//! metadata (Preface, ContentStorage, the file SourcePackage, its Tracks, Sequences, the
//! descriptors) it learns what each essence element key means; the essence elements give the
//! flat sample table of each track; index segments give key frames and the temporal offsets of
//! Long GOP streams.
//!
//! * Frame wrapping: one essence element per frame (video) or per edit unit (sound); the sample
//!   offset is the element's value, so MXF's own KLV never reaches a decoder.
//! * Clip wrapping (Avid OP-Atom, BWF clip elements): one element for the whole track, cut into
//!   frames by the index table (constant `EditUnitByteCount`, or the entries' stream offsets),
//!   and PCM cut into blocks of one edit unit.
//! * Partitions: essence in any number of body partitions, the metadata of the last header copy
//!   that describes tracks wins (the footer's copy in a file that grew while recorded), and a
//!   file cut in the middle of an element keeps the elements before it. No footer or index is
//!   needed: the sample table comes from the elements themselves.
//! * Run-in (up to 64 KiB before the header partition) is skipped.
//!
//! H.264 and HEVC essence is an Annex B byte stream with in-band parameter sets; the track gets
//! an `avcC` / `hvcC` made from the first frames that carry them, and [`Track::read_sample`]
//! turns each frame into length-prefixed NAL units (see [`crate::annexb`]). MPEG-2 and JPEG 2000
//! are listed (codec, size, sample table) but have no decoder yet.
//!
//! OP-Atom: one essence per file. The video file's [`Mxf::material_package`] is the id the
//! other files of the clip (the audio `.mxf`s) share, and [`Mxf::related_packages`] names the
//! file packages the material package points to; linking the files is left to the caller.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use crate::annexb;
use crate::mp4::{Codec, Kind, Movie, Sample, Track};
use crate::{Error, Result};

const KLV: [u8; 4] = [0x06, 0x0e, 0x2b, 0x34];
/// Run-in at most this long in front of the header partition.
const RUN_IN_MAX: usize = 65535;
const SAMPLES_MAX: usize = 50_000_000;
const SET_MAX: u64 = 256 << 20;
const CHUNK: usize = 1 << 16;

/// Whether `head` (the first bytes of a file, at least 12) starts an MXF file: a partition pack
/// key at 0, or (after a run-in) the header partition key within the first 64 KiB.
pub fn is_mxf(head: &[u8]) -> bool {
    const PACK: [u8; 11] = [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02];
    if head.len() < PACK.len() {
        return false;
    }
    if head.starts_with(&PACK) {
        return true;
    }
    // a run-in is never an ISO base media or EBML file
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"mdat" | b"free" | b"wide" | b"skip" | b"pnot") {
        return false;
    }
    head.windows(PACK.len()).take(RUN_IN_MAX + 1).any(|w| w == PACK)
}

/// [`is_mxf`] on the start of `r`; the position goes back to 0.
pub fn sniff<R: Read + Seek>(r: &mut R) -> Result<bool> {
    let mut head = Vec::with_capacity(RUN_IN_MAX + 16);
    r.seek(SeekFrom::Start(0))?;
    r.take((RUN_IN_MAX + 16) as u64).read_to_end(&mut head)?;
    r.seek(SeekFrom::Start(0))?;
    Ok(is_mxf(&head))
}

/// What kind of partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKind {
    Header,
    Body,
    Footer,
}

/// A partition pack (ST 377-1, 7.1).
#[derive(Debug, Clone)]
pub struct Partition {
    /// Position of the pack's key in the file.
    pub pos: u64,
    pub kind: PartitionKind,
    /// The pack says the partition is closed (header metadata final).
    pub closed: bool,
    /// The pack says the metadata is complete.
    pub complete: bool,
    pub kag_size: u32,
    pub previous_partition: u64,
    pub footer_partition: u64,
    pub header_byte_count: u64,
    pub index_byte_count: u64,
    pub index_sid: u32,
    /// Byte offset of this partition's essence in its essence container.
    pub body_offset: u64,
    pub body_sid: u32,
    pub operational_pattern: [u8; 16],
    pub essence_containers: Vec<[u8; 16]>,
}

/// The operational pattern named by a partition pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalPattern {
    /// OP1a, OP1b, OP2a, … : (item complexity 1..3, package complexity 'a'..'c' as 1..3).
    Generic { item: u8, package: u8 },
    /// OP-Atom: one essence in the file.
    Atom,
    Other,
}

impl OperationalPattern {
    pub fn from_label(ul: &[u8; 16]) -> Self {
        if ul[8..12] != [0x0d, 0x01, 0x02, 0x01] {
            return Self::Other;
        }
        match ul[12] {
            0x10 => Self::Atom,
            item @ 1..=3 if (1..=3).contains(&ul[13]) => Self::Generic { item, package: ul[13] },
            _ => Self::Other,
        }
    }
    pub fn is_op1a(&self) -> bool {
        *self == Self::Generic { item: 1, package: 1 }
    }
}

/// How the frames of a track lie in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrapping {
    /// One essence element per edit unit.
    Frame,
    /// One essence element for the whole track (cut by the index table).
    Clip,
}

/// What a picture track holds, from the picture essence coding label and the essence container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureCodec {
    Avc,
    Hevc,
    /// SMPTE VC-3 (DNxHD, DNxHR).
    Vc3,
    /// SMPTE RDD 44; the profile 1 (Proxy) .. 6 (4444 XQ) when the label names it.
    ProRes(u8),
    Mpeg2,
    Jpeg2000,
    Uncompressed,
    Other,
}

/// Picture descriptor values (GenericPictureEssenceDescriptor and CDCI / RGBA).
#[derive(Debug, Clone, Default)]
pub struct PictureDesc {
    pub stored_width: u32,
    pub stored_height: u32,
    pub display_width: Option<u32>,
    pub display_height: Option<u32>,
    /// Display aspect ratio.
    pub aspect: Option<(i32, i32)>,
    /// 0 full frame, 1 separate fields, 2 single field, 3 mixed fields, 4 segmented frame.
    pub frame_layout: u8,
    pub component_depth: u32,
    pub h_subsampling: u32,
    pub v_subsampling: u32,
    pub rgba: bool,
    pub coding: [u8; 16],
}

/// Sound descriptor values (GenericSoundEssenceDescriptor, WaveAudio, AES3Audio).
#[derive(Debug, Clone, Default)]
pub struct SoundDesc {
    pub sample_rate: (i32, i32),
    pub channels: u32,
    pub bits: u32,
    pub block_align: u32,
    pub coding: [u8; 16],
    pub aes3: bool,
}

/// One essence track of the file package.
#[derive(Debug, Clone)]
pub struct MxfTrack {
    pub track_id: u32,
    /// The essence element key's last four bytes this track is stored under (0: not stated).
    pub track_number: u32,
    pub kind: Kind,
    /// Edit rate of the track (frames per second, or the sample rate of clip-wrapped sound).
    pub edit_rate: (i32, i32),
    pub origin: i64,
    /// Duration the Sequence states, in edit units.
    pub duration: i64,
    pub essence_container: [u8; 16],
    pub picture: Option<PictureDesc>,
    pub sound: Option<SoundDesc>,
    pub codec: Option<PictureCodec>,
    pub wrapping: Wrapping,
    /// Samples found in the file.
    pub samples: usize,
}

/// What [`parse`] found.
#[derive(Debug, Clone)]
pub struct Mxf {
    /// Bytes in front of the header partition.
    pub run_in: u64,
    pub partitions: Vec<Partition>,
    pub pattern: OperationalPattern,
    /// Essence container labels the header partition lists.
    pub essence_containers: Vec<[u8; 16]>,
    /// Local tag to UL, from the primer pack of the metadata in use.
    pub primer: Vec<(u16, [u8; 16])>,
    pub tracks: Vec<MxfTrack>,
    /// Index table segments found (in all partitions).
    pub index_segments: usize,
    /// Entries of the index table in use, the ones with a temporal offset, and the ones flagged random access.
    pub index_entries: (usize, usize, usize),
    /// Constant edit unit byte count of the index table (0: variable).
    pub edit_unit_byte_count: u32,
    /// UMID of the file package, and of the material package (shared by the files of an OP-Atom clip).
    pub file_package: Option<[u8; 32]>,
    pub material_package: Option<[u8; 32]>,
    /// File packages the material package's source clips name.
    pub related_packages: Vec<[u8; 32]>,
    /// The header said the file was still being written, or the scan stopped in a cut element.
    pub incomplete: bool,
    pub movie: Movie,
}

// ---------------------------------------------------------------------------------------------
// reading

struct Src<R> {
    r: R,
    len: u64,
    buf: Vec<u8>,
    start: u64,
}

impl<R: Read + Seek> Src<R> {
    fn new(mut r: R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        Ok(Self { r, len, buf: Vec::new(), start: 0 })
    }

    /// Up to `n` bytes at `pos` (fewer at the end of the file).
    fn get(&mut self, pos: u64, n: usize) -> Result<&[u8]> {
        if pos >= self.len {
            return Ok(&[]);
        }
        let end = pos.saturating_add(n as u64).min(self.len);
        let want = (end - pos) as usize;
        if !(pos >= self.start && end <= self.start + self.buf.len() as u64) {
            let chunk = want.max(CHUNK).min((self.len - pos) as usize);
            self.r.seek(SeekFrom::Start(pos))?;
            self.buf.resize(chunk, 0);
            self.r.read_exact(&mut self.buf)?;
            self.start = pos;
        }
        let o = (pos - self.start) as usize;
        Ok(&self.buf[o..o + want])
    }
}

fn u16be(b: &[u8], p: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(p..p + 2)?.try_into().ok()?))
}
fn u32be(b: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(p..p + 4)?.try_into().ok()?))
}
fn u64be(b: &[u8], p: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(p..p + 8)?.try_into().ok()?))
}

/// BER length at the start of `b`: (value, bytes used).
fn ber(b: &[u8]) -> Option<(u64, usize)> {
    let f = *b.first()?;
    if f < 0x80 {
        return Some((f as u64, 1));
    }
    let n = (f & 0x7f) as usize;
    if n == 0 || n > 8 || b.len() < 1 + n {
        return None;
    }
    Some((b[1..=n].iter().fold(0u64, |a, &x| (a << 8) | x as u64), 1 + n))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    Partition(PartitionKind, u8),
    Primer,
    Rip,
    Index,
    /// A header metadata set; the set's kind byte.
    Set(u8),
    /// A generic container element (key bytes 12..16 identify it).
    Essence,
    Fill,
    Other,
}

fn classify(k: &[u8]) -> KeyKind {
    if k[..4] != KLV {
        return KeyKind::Other;
    }
    if k[4..13] == [0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01] {
        match k[13] {
            0x02 => return KeyKind::Partition(PartitionKind::Header, k[14]),
            0x03 => return KeyKind::Partition(PartitionKind::Body, k[14]),
            0x04 => return KeyKind::Partition(PartitionKind::Footer, k[14]),
            0x05 => return KeyKind::Primer,
            0x11 => return KeyKind::Rip,
            _ => {}
        }
    }
    if k[4..8] == [0x02, 0x53, 0x01, 0x01] && k[8..13] == [0x0d, 0x01, 0x02, 0x01, 0x01] && k[13] == 0x10 {
        return KeyKind::Index;
    }
    if k[4] == 0x02 && k[5] == 0x53 && k[8..14] == [0x0d, 0x01, 0x01, 0x01, 0x01, 0x01] {
        return KeyKind::Set(k[14]);
    }
    if k[4] == 0x01 && k[5] == 0x02 && k[8..12] == [0x0d, 0x01, 0x03, 0x01] {
        return KeyKind::Essence;
    }
    if k[4] == 0x01 && k[5] == 0x01 && k[6] == 0x01 && k[8..12] == [0x03, 0x01, 0x02, 0x10] {
        return KeyKind::Fill;
    }
    KeyKind::Other
}

fn parse_partition(pos: u64, kind: PartitionKind, status: u8, v: &[u8]) -> Option<Partition> {
    let n = u32be(v, 80)? as usize;
    let item = u32be(v, 84)? as usize;
    let mut essence_containers = Vec::new();
    if item == 16 {
        for i in 0..n.min((v.len().saturating_sub(88)) / 16) {
            essence_containers.push(v[88 + i * 16..88 + i * 16 + 16].try_into().ok()?);
        }
    }
    Some(Partition {
        pos,
        kind,
        closed: status == 2 || status == 4,
        complete: status == 3 || status == 4,
        kag_size: u32be(v, 4)?,
        previous_partition: u64be(v, 16)?,
        footer_partition: u64be(v, 24)?,
        header_byte_count: u64be(v, 32)?,
        index_byte_count: u64be(v, 40)?,
        index_sid: u32be(v, 48)?,
        body_offset: u64be(v, 52)?,
        body_sid: u32be(v, 60)?,
        operational_pattern: v.get(64..80)?.try_into().ok()?,
        essence_containers,
    })
}

// ---------------------------------------------------------------------------------------------
// header metadata

/// A local set: its kind byte (the UL's 15th byte) and its properties by local tag.
#[derive(Debug, Clone, Default)]
struct Set {
    kind: u8,
    props: Vec<(u16, Vec<u8>)>,
}

impl Set {
    fn get(&self, tag: u16) -> Option<&[u8]> {
        self.props.iter().find(|p| p.0 == tag).map(|p| p.1.as_slice())
    }
    fn u8(&self, tag: u16) -> Option<u8> {
        self.get(tag)?.first().copied()
    }
    fn u16(&self, tag: u16) -> Option<u16> {
        u16be(self.get(tag)?, 0)
    }
    fn u32(&self, tag: u16) -> Option<u32> {
        u32be(self.get(tag)?, 0)
    }
    fn i64(&self, tag: u16) -> Option<i64> {
        Some(u64be(self.get(tag)?, 0)? as i64)
    }
    fn rational(&self, tag: u16) -> Option<(i32, i32)> {
        let b = self.get(tag)?;
        Some((u32be(b, 0)? as i32, u32be(b, 4)? as i32))
    }
    fn ul(&self, tag: u16) -> Option<[u8; 16]> {
        self.get(tag)?.get(..16)?.try_into().ok()
    }
    fn uid(&self, tag: u16) -> Option<[u8; 16]> {
        self.ul(tag)
    }
    fn umid(&self, tag: u16) -> Option<[u8; 32]> {
        self.get(tag)?.get(..32)?.try_into().ok()
    }
    /// A batch / array of 16-byte references.
    fn refs(&self, tag: u16) -> Vec<[u8; 16]> {
        let Some(b) = self.get(tag) else { return Vec::new() };
        let (Some(n), Some(sz)) = (u32be(b, 0), u32be(b, 4)) else { return Vec::new() };
        if sz != 16 {
            return Vec::new();
        }
        (0..n as usize).filter_map(|i| b.get(8 + i * 16..8 + i * 16 + 16)?.try_into().ok()).collect()
    }
}

fn parse_set(v: &[u8], kind: u8) -> Option<([u8; 16], Set)> {
    let mut props = Vec::new();
    let mut p = 0usize;
    let mut uid = None;
    while p + 4 <= v.len() {
        let tag = u16be(v, p)?;
        let len = u16be(v, p + 2)? as usize;
        let val = v.get(p + 4..p + 4 + len)?;
        if tag == 0x3c0a && len == 16 {
            uid = val.try_into().ok();
        }
        props.push((tag, val.to_vec()));
        p += 4 + len;
    }
    Some((uid?, Set { kind, props }))
}

/// One generation of header metadata: the primer and the sets after it.
#[derive(Default)]
struct Meta {
    primer: Vec<(u16, [u8; 16])>,
    sets: HashMap<[u8; 16], Set>,
}

fn parse_primer(v: &[u8]) -> Vec<(u16, [u8; 16])> {
    let (Some(n), Some(sz)) = (u32be(v, 0), u32be(v, 4)) else { return Vec::new() };
    if sz != 18 {
        return Vec::new();
    }
    (0..n as usize).filter_map(|i| Some((u16be(v, 8 + i * 18)?, v.get(10 + i * 18..26 + i * 18)?.try_into().ok()?))).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Def {
    Picture,
    Sound,
    Data,
    Timecode,
    Other,
}

fn data_def(ul: &[u8; 16]) -> Def {
    match (&ul[8..12], ul[12]) {
        ([0x01, 0x03, 0x02, 0x02], 1) => Def::Picture,
        ([0x01, 0x03, 0x02, 0x02], 2) => Def::Sound,
        ([0x01, 0x03, 0x02, 0x02], 3) => Def::Data,
        ([0x01, 0x03, 0x02, 0x01], _) => Def::Timecode,
        _ => Def::Other,
    }
}

struct Resolved {
    tracks: Vec<MxfTrack>,
    file_package: Option<[u8; 32]>,
    material_package: Option<[u8; 32]>,
    related: Vec<[u8; 32]>,
    /// Packages' essence container (IndexSID, BodySID) pairs.
    #[allow(dead_code)]
    containers: Vec<(u32, u32)>,
}

fn picture_desc(s: &Set) -> Option<PictureDesc> {
    let w = s.u32(0x3203)?;
    let h = s.u32(0x3202)?;
    Some(PictureDesc {
        stored_width: w,
        stored_height: h,
        display_width: s.u32(0x3209),
        display_height: s.u32(0x3208),
        aspect: s.rational(0x320e),
        frame_layout: s.u8(0x320c).unwrap_or(0),
        component_depth: s.u32(0x3301).unwrap_or(0),
        h_subsampling: s.u32(0x3302).unwrap_or(0),
        v_subsampling: s.u32(0x3308).unwrap_or(0),
        rgba: s.kind == 0x29,
        coding: s.ul(0x3201).unwrap_or([0; 16]),
    })
}

fn sound_desc(s: &Set) -> Option<SoundDesc> {
    let channels = s.u32(0x3d07)?;
    Some(SoundDesc {
        sample_rate: s.rational(0x3d03).unwrap_or((0, 1)),
        channels,
        bits: s.u32(0x3d01).unwrap_or(0),
        block_align: s.u16(0x3d0a).map_or(0, |v| v as u32),
        coding: s.ul(0x3d06).unwrap_or([0; 16]),
        aes3: s.kind == 0x47,
    })
}

fn resolve(m: &Meta) -> Option<Resolved> {
    let preface = m.sets.values().find(|s| s.kind == 0x2f)?;
    let storage = m.sets.get(&preface.uid(0x3b03)?)?;
    let packages: Vec<&Set> = storage.refs(0x1901).iter().filter_map(|u| m.sets.get(u)).collect();
    let mut containers = Vec::new();
    let mut linked: Vec<[u8; 32]> = Vec::new();
    for u in storage.refs(0x1902) {
        if let Some(c) = m.sets.get(&u) {
            containers.push((c.u32(0x3f06).unwrap_or(0), c.u32(0x3f07).unwrap_or(0)));
            if let Some(id) = c.umid(0x2701) {
                linked.push(id);
            }
        }
    }
    let material = packages.iter().find(|p| p.kind == 0x36);
    // file packages: source packages with a picture or sound descriptor
    // (the ones the essence container data names first)
    let mut cands: Vec<(&Set, Vec<&Set>)> = Vec::new();
    for p in packages.iter().filter(|p| p.kind == 0x37) {
        let Some(d) = p.uid(0x4701).and_then(|u| m.sets.get(&u)) else { continue };
        let descs: Vec<&Set> = if d.kind == 0x44 { d.refs(0x3f01).iter().filter_map(|u| m.sets.get(u)).collect() } else { vec![d] };
        let descs: Vec<&Set> = descs.into_iter().filter(|d| d.get(0x3203).is_some() || d.get(0x3d07).is_some()).collect();
        if !descs.is_empty() {
            cands.push((p, descs));
        }
    }
    let pick = cands.iter().position(|(p, _)| p.umid(0x4401).is_some_and(|id| linked.contains(&id))).or(if cands.is_empty() { None } else { Some(0) });
    let (pkg, descs) = pick.map(|i| cands.swap_remove(i))?;
    let mut tracks = Vec::new();
    for tu in pkg.refs(0x4403) {
        let Some(t) = m.sets.get(&tu) else { continue };
        if t.kind != 0x3b {
            continue;
        }
        let Some(seq) = t.uid(0x4803).and_then(|u| m.sets.get(&u)) else { continue };
        let def = seq.ul(0x0201).map_or(Def::Other, |u| data_def(&u));
        if !matches!(def, Def::Picture | Def::Sound) {
            continue;
        }
        let track_id = t.u32(0x4801).unwrap_or(0);
        // the descriptor of this track: by LinkedTrackID, else the only one of its kind
        let want_pic = def == Def::Picture;
        let desc = descs
            .iter()
            .find(|d| d.u32(0x3006) == Some(track_id) && (d.get(0x3203).is_some() == want_pic))
            .or_else(|| descs.iter().find(|d| d.get(0x3203).is_some() == want_pic))?;
        let picture = if want_pic { picture_desc(desc) } else { None };
        let sound = if want_pic { None } else { sound_desc(desc) };
        if picture.is_none() && sound.is_none() {
            continue;
        }
        let ec = desc.ul(0x3004).unwrap_or([0; 16]);
        tracks.push(MxfTrack {
            track_id,
            track_number: t.u32(0x4804).unwrap_or(0),
            kind: if want_pic { Kind::Video } else { Kind::Audio },
            edit_rate: t.rational(0x4b01).filter(|r| r.0 > 0 && r.1 > 0).or_else(|| desc.rational(0x3001)).unwrap_or((25, 1)),
            origin: t.i64(0x4b02).unwrap_or(0),
            duration: seq.i64(0x0202).unwrap_or(0),
            essence_container: ec,
            codec: picture.as_ref().map(|p| classify_picture(&p.coding, &ec)),
            picture,
            sound,
            wrapping: Wrapping::Frame,
            samples: 0,
        });
    }
    if tracks.is_empty() {
        return None;
    }
    let mut related = Vec::new();
    if let Some(mp) = material {
        for tu in mp.refs(0x4403) {
            let Some(seq) = m.sets.get(&tu).and_then(|t| t.uid(0x4803)).and_then(|u| m.sets.get(&u)) else { continue };
            for cu in seq.refs(0x1001) {
                if let Some(id) = m.sets.get(&cu).and_then(|c| c.umid(0x1101)) {
                    if id != [0; 32] && !related.contains(&id) {
                        related.push(id);
                    }
                }
            }
        }
    }
    Some(Resolved { tracks, file_package: pkg.umid(0x4401), material_package: material.and_then(|p| p.umid(0x4401)), related, containers })
}

/// The codec from the picture essence coding label, then the essence container label.
pub fn classify_picture(coding: &[u8; 16], container: &[u8; 16]) -> PictureCodec {
    // the AVC byte stream container label (…02 10 xx xx) beats a coding label from the MPEG family
    if container[8..14] == [0x0d, 0x01, 0x03, 0x01, 0x02, 0x10] {
        return PictureCodec::Avc;
    }
    if coding[8..12] == [0x04, 0x01, 0x02, 0x02] {
        match (coding[12], coding[13]) {
            // 0x31 is what ffmpeg writes, 0x32 Sony's XAVC and the standard's AVC
            (0x01, 0x31 | 0x32) => return PictureCodec::Avc,
            (0x01, 0x33..=0x3f) => return PictureCodec::Hevc,
            (0x01, _) => return PictureCodec::Mpeg2,
            (0x71, _) => return PictureCodec::Vc3,
            (0x03, 0x06) => return PictureCodec::ProRes(coding[14]),
            (0x03, _) => return PictureCodec::Jpeg2000,
            _ => {}
        }
    }
    if coding[8..11] == [0x04, 0x01, 0x02] && coding[11] == 0x01 {
        return PictureCodec::Uncompressed;
    }
    if container[8..13] == [0x0d, 0x01, 0x03, 0x01, 0x02] {
        match container[13] {
            0x04 => return PictureCodec::Mpeg2,
            0x05 => return PictureCodec::Uncompressed,
            0x0c => return PictureCodec::Jpeg2000,
            0x10 => return PictureCodec::Avc,
            0x11 => return PictureCodec::Vc3,
            _ => {}
        }
    }
    PictureCodec::Other
}

// ---------------------------------------------------------------------------------------------
// index tables

#[derive(Debug, Clone, Copy)]
struct IdxEntry {
    temporal_offset: i8,
    key_frame_offset: i8,
    flags: u8,
    stream_offset: u64,
}

#[derive(Debug, Clone, Default)]
struct IndexSeg {
    sid: u32,
    rate: (i32, i32),
    start: i64,
    edit_unit_byte_count: u32,
    entries: Vec<IdxEntry>,
}

fn parse_index(v: &[u8]) -> Option<IndexSeg> {
    let mut seg = IndexSeg::default();
    let mut slices = 0usize;
    let mut pos_tables = 0usize;
    let mut entries_raw: Option<&[u8]> = None;
    let mut p = 0usize;
    while p + 4 <= v.len() {
        let tag = u16be(v, p)?;
        let len = u16be(v, p + 2)? as usize;
        // IndexEntryArray can be longer than 64 KiB in files that write a 4-byte local length;
        // the standard's local length is 16 bit, so a longer array is never written this way
        let val = v.get(p + 4..p + 4 + len)?;
        match tag {
            0x3f0b => seg.rate = (u32be(val, 0)? as i32, u32be(val, 4)? as i32),
            0x3f0c => seg.start = u64be(val, 0)? as i64,
            0x3f05 => seg.edit_unit_byte_count = u32be(val, 0)?,
            0x3f06 => seg.sid = u32be(val, 0)?,
            0x3f08 => slices = *val.first()? as usize,
            0x3f0e => pos_tables = *val.first()? as usize,
            0x3f0a => entries_raw = Some(val),
            _ => {}
        }
        p += 4 + len;
    }
    if let Some(b) = entries_raw {
        let n = u32be(b, 0)? as usize;
        let sz = u32be(b, 4)? as usize;
        let min = 11 + 4 * slices + 8 * pos_tables;
        if sz < min {
            return None;
        }
        for i in 0..n.min(b.len().saturating_sub(8) / sz) {
            let e = &b[8 + i * sz..];
            seg.entries.push(IdxEntry { temporal_offset: e[0] as i8, key_frame_offset: e[1] as i8, flags: e[2], stream_offset: u64be(e, 3)? });
        }
    }
    Some(seg)
}

// ---------------------------------------------------------------------------------------------
// scanning

#[derive(Debug, Clone, Copy)]
struct Elem {
    /// File position of the value.
    off: u64,
    len: u64,
}

struct Group {
    key: [u8; 4],
    elems: Vec<Elem>,
}

/// Maps a byte position of a clip-wrapped essence container's concatenated values to the file.
struct Stream<'a> {
    elems: &'a [Elem],
    /// Start of each element in the stream.
    starts: Vec<u64>,
    total: u64,
}

impl<'a> Stream<'a> {
    fn new(elems: &'a [Elem]) -> Self {
        let mut starts = Vec::with_capacity(elems.len());
        let mut t = 0u64;
        for e in elems {
            starts.push(t);
            t += e.len;
        }
        Self { elems, starts, total: t }
    }
    /// File position of stream byte `s` and how many bytes follow contiguously in the file.
    fn at(&self, s: u64) -> Option<(u64, u64)> {
        let i = self.starts.partition_point(|&x| x <= s).checked_sub(1)?;
        let e = self.elems[i];
        let d = s - self.starts[i];
        (d < e.len).then(|| (e.off + d, e.len - d))
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Sniffs the picture type of one Annex B / MPEG-2 frame for key frames without an index.
fn sniff_key(codec: PictureCodec, data: &[u8]) -> bool {
    match codec {
        PictureCodec::Avc => annexb::nal_ranges(data).iter().any(|&(s, _)| matches!(annexb::h264_type(&data[s..]), 5)),
        PictureCodec::Hevc => annexb::nal_ranges(data).iter().any(|&(s, _)| (16..=21).contains(&annexb::hevc_type(&data[s..]))),
        PictureCodec::Mpeg2 => data.windows(4).any(|w| w == [0, 0, 1, 0xb3] || w == [0, 0, 1, 0xb8]),
        _ => true,
    }
}

/// Parses an MXF file.
pub fn parse<R: Read + Seek>(r: &mut R) -> Result<Mxf> {
    let mut src = Src::new(&mut *r)?;
    // run-in
    let head = src.get(0, RUN_IN_MAX + 16)?.to_vec();
    let run_in = if head.len() >= 4 && head[..4] == KLV {
        0
    } else {
        const PACK: [u8; 11] = [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02];
        head.windows(PACK.len()).position(|w| w == PACK).ok_or(Error::Invalid("no MXF partition pack"))? as u64
    };
    drop(head);

    let mut partitions: Vec<Partition> = Vec::new();
    let mut gens: Vec<Meta> = vec![Meta::default()];
    let mut groups: Vec<Group> = Vec::new();
    let mut group_at: HashMap<[u8; 4], usize> = HashMap::new();
    let mut index: Vec<IndexSeg> = Vec::new();
    let mut incomplete = false;
    let mut pos = run_in;
    loop {
        let (key, vstart, len) = {
            let h = src.get(pos, 25)?;
            if h.len() < 17 || h[..4] != KLV {
                break;
            }
            let Some((len, n)) = ber(&h[16..]) else { break };
            let mut key = [0u8; 16];
            key.copy_from_slice(&h[..16]);
            (key, pos + 16 + n as u64, len)
        };
        let vend = vstart.saturating_add(len);
        let cut = vend > src.len;
        let kind = classify(&key);
        match kind {
            KeyKind::Rip => break,
            KeyKind::Essence => {
                if cut {
                    incomplete = true;
                    break;
                }
                // system items (key byte 12 = 0x04 / 0x14) carry no track
                if !matches!(key[12], 0x04 | 0x14) {
                    let k: [u8; 4] = key[12..16].try_into().unwrap();
                    let gi = *group_at.entry(k).or_insert_with(|| {
                        groups.push(Group { key: k, elems: Vec::new() });
                        groups.len() - 1
                    });
                    if groups[gi].elems.len() < SAMPLES_MAX {
                        groups[gi].elems.push(Elem { off: vstart, len });
                    }
                }
            }
            KeyKind::Partition(pk, status) => {
                if cut || len > SET_MAX {
                    break;
                }
                let v = src.get(vstart, len as usize)?;
                if let Some(p) = parse_partition(pos - 0, pk, status, v) {
                    if matches!(pk, PartitionKind::Header) || partitions.is_empty() {
                        incomplete |= !p.closed || !p.complete;
                    }
                    partitions.push(p);
                }
            }
            KeyKind::Primer | KeyKind::Set(_) | KeyKind::Index => {
                if cut || len > SET_MAX {
                    incomplete = true;
                    break;
                }
                let v = src.get(vstart, len as usize)?.to_vec();
                match kind {
                    KeyKind::Primer => {
                        if !gens.last().is_some_and(|g| g.sets.is_empty() && g.primer.is_empty()) {
                            gens.push(Meta::default());
                        }
                        gens.last_mut().unwrap().primer = parse_primer(&v);
                    }
                    KeyKind::Set(k) => {
                        if let Some((uid, s)) = parse_set(&v, k) {
                            gens.last_mut().unwrap().sets.insert(uid, s);
                        }
                    }
                    _ => {
                        if let Some(seg) = parse_index(&v) {
                            index.push(seg);
                        }
                    }
                }
            }
            KeyKind::Fill | KeyKind::Other => {}
        }
        pos = vend;
        if pos >= src.len {
            break;
        }
    }

    // the last generation of header metadata that describes tracks
    let mut resolved = None;
    let mut primer = Vec::new();
    for g in gens.iter().rev() {
        if let Some(r) = resolve(g) {
            primer = g.primer.clone();
            resolved = Some(r);
            break;
        }
    }
    let Some(res) = resolved else { return Err(Error::Invalid("MXF: no track with an essence descriptor")) };

    let pattern = partitions.first().map_or(OperationalPattern::Other, |p| OperationalPattern::from_label(&p.operational_pattern));
    let essence_containers = partitions.first().map(|p| p.essence_containers.clone()).unwrap_or_default();

    // index segments of the essence container that has the most entries
    let mut by_sid: HashMap<u32, usize> = HashMap::new();
    for s in &index {
        *by_sid.entry(s.sid).or_default() += s.entries.len().max(1);
    }
    let sid = by_sid.iter().max_by_key(|(_, n)| **n).map(|(s, _)| *s);
    let mut segs: Vec<&IndexSeg> = index.iter().filter(|s| Some(s.sid) == sid).collect();
    segs.sort_by_key(|s| s.start);
    let index_rate = segs.first().map(|s| s.rate).unwrap_or((0, 0));
    let ebc = segs.iter().map(|s| s.edit_unit_byte_count).find(|&e| e > 0).unwrap_or(0);
    let mut entries: Vec<Option<IdxEntry>> = Vec::new();
    for s in &segs {
        for (i, e) in s.entries.iter().enumerate() {
            let at = s.start + i as i64;
            if (0..SAMPLES_MAX as i64).contains(&at) {
                if entries.len() <= at as usize {
                    entries.resize(at as usize + 1, None);
                }
                entries[at as usize] = Some(*e);
            }
        }
    }
    let have_entries = entries.iter().filter(|e| e.is_some()).count();
    // key frame: it points at itself and no prediction flags (0x30 forward / backward, 0x03) are set
    let entry_key = |e: &IdxEntry| e.key_frame_offset == 0 && e.flags & 0x33 == 0;

    // essence groups to tracks: by track number, then by kind in order of appearance
    let mut tracks = res.tracks;
    let mut used = vec![false; groups.len()];
    let mut assigned: Vec<Option<usize>> = vec![None; tracks.len()];
    for (ti, t) in tracks.iter().enumerate() {
        if t.track_number != 0 {
            if let Some(gi) = groups.iter().enumerate().position(|(gi, g)| !used[gi] && u32::from_be_bytes(g.key) == t.track_number) {
                used[gi] = true;
                assigned[ti] = Some(gi);
            }
        }
    }
    for (ti, t) in tracks.iter().enumerate() {
        if assigned[ti].is_some() {
            continue;
        }
        let items: &[u8] = if t.kind == Kind::Video { &[0x05, 0x15] } else { &[0x06, 0x16] };
        if let Some(gi) = groups.iter().enumerate().position(|(gi, g)| !used[gi] && items.contains(&g.key[0])) {
            used[gi] = true;
            assigned[ti] = Some(gi);
        }
    }

    let video_rate = tracks.iter().find(|t| t.kind == Kind::Video).map(|t| t.edit_rate);
    let mut out_tracks: Vec<Track> = Vec::new();
    let mut movie_timescale = 0u32;
    let mut movie_duration = 0u64;
    for (ti, t) in tracks.iter_mut().enumerate() {
        let Some(gi) = assigned[ti] else { continue };
        let g = &groups[gi];
        let rate = (t.edit_rate.0.max(1) as u64, t.edit_rate.1.max(1) as u64);
        let index_matches = have_entries > 0 && index_rate.0 as i64 * rate.1 as i64 == rate.0 as i64 * index_rate.1 as i64;
        let (samples, codec, fourcc, timescale) = if t.kind == Kind::Video {
            let pd = t.picture.clone().unwrap_or_default();
            let pc = t.codec.unwrap_or(PictureCodec::Other);
            // wrapping
            let label_clip = t.essence_container[8..13] == [0x0d, 0x01, 0x03, 0x01, 0x02] && matches!(t.essence_container[13], 0x0c | 0x11) && t.essence_container[14] == 2;
            let single = g.elems.len() == 1 && (ebc > 0 || have_entries >= 2);
            let clip = (label_clip || single) && !g.elems.is_empty();
            t.wrapping = if clip { Wrapping::Clip } else { Wrapping::Frame };
            let den = rate.1 as i64;
            let intra = matches!(pc, PictureCodec::Vc3 | PictureCodec::ProRes(_) | PictureCodec::Uncompressed | PictureCodec::Jpeg2000);
            // a frame: where it is, how big, and whether it is a key frame
            let mut frames: Vec<(u64, u32, bool)> = Vec::new();
            if clip {
                let st = Stream::new(&g.elems);
                if ebc > 0 {
                    let n = (st.total / ebc as u64) as usize;
                    for i in 0..n.min(SAMPLES_MAX) {
                        let Some((off, _)) = st.at(i as u64 * ebc as u64) else { break };
                        frames.push((off, ebc, true));
                    }
                } else {
                    let list: Vec<&IdxEntry> = entries.iter().flatten().collect();
                    for (i, e) in list.iter().enumerate() {
                        let o = e.stream_offset;
                        let end = list.get(i + 1).map_or(st.total, |n| n.stream_offset);
                        let Some((off, _)) = st.at(o) else { break };
                        if end <= o || end > st.total || end - o > u32::MAX as u64 {
                            break;
                        }
                        frames.push((off, (end - o) as u32, intra || entry_key(e)));
                    }
                }
            } else {
                for (n, e) in g.elems.iter().enumerate() {
                    if e.len > u32::MAX as u64 {
                        break;
                    }
                    let entry = if index_matches { entries.get(n).copied().flatten() } else { None };
                    let key = match entry {
                        Some(x) => intra || entry_key(&x),
                        None => match pc {
                            PictureCodec::Avc | PictureCodec::Hevc | PictureCodec::Mpeg2 => {
                                let d = src.get(e.off, (e.len as usize).min(16384))?;
                                sniff_key(pc, d)
                            }
                            _ => true,
                        },
                    };
                    frames.push((e.off, e.len as u32, key));
                }
            }
            // Presentation times from the temporal offsets, read as ffmpeg's demuxer does (it
            // reads Sony's and Panasonic's files, and its own): the entry at position x says that
            // the picture stored at x + offset is shown at x. Decode times are the stored
            // position moved back by the largest offset, so that no picture is shown before it
            // is decoded: the first picture is shown at 0 and decoded before, as in an MP4 with
            // a `ctts` and an edit list. Without offsets, shown = decoded.
            let total = frames.len();
            let mut shown: Vec<i64> = (0..total as i64).collect();
            let mut delay = 0i64;
            if (index_matches || clip) && have_entries > 0 && !intra {
                let mut inv: Vec<Option<i64>> = vec![None; total];
                for (x, e) in entries.iter().enumerate() {
                    let Some(e) = e else { continue };
                    delay = delay.max(e.temporal_offset as i64);
                    let s = x as i64 + e.temporal_offset as i64;
                    if (0..total as i64).contains(&s) {
                        inv[s as usize] = Some(x as i64);
                    }
                }
                if inv.iter().all(|v| v.is_some()) {
                    for (s, v) in inv.iter().enumerate() {
                        shown[s] = v.unwrap();
                    }
                } else {
                    delay = 0;
                }
            }
            let samples: Vec<Sample> = frames.iter().enumerate().map(|(n, f)| Sample { offset: f.0, size: f.1, dts: (n as i64 - delay) * den, pts: shown[n] * den, key: f.2 }).collect();
            let (codec, fourcc) = video_codec(&mut src, pc, &pd, &samples)?;
            (samples, codec, fourcc, rate.0 as u32)
        } else {
            let sd = t.sound.clone().unwrap_or_default();
            let sr = if sd.sample_rate.0 > 0 && sd.sample_rate.1 > 0 { (sd.sample_rate.0 as f64 / sd.sample_rate.1 as f64).round() as u32 } else { 48000 };
            let ch = sd.channels.max(1) as usize;
            let bytes = if sd.block_align >= sd.channels && sd.channels > 0 { (sd.block_align / sd.channels).clamp(1, 4) } else { sd.bits.div_ceil(8).clamp(1, 4) } as usize;
            let key_type = g.key[2];
            let clip_key = matches!(key_type, 2 | 4);
            // AES3 elements carry a 4-byte header with the sample count (D-10, SMPTE 386M); some
            // writers give the AES3 labels to plain interleaved PCM, so the header has to fit
            // the element's size in the first elements
            let mut aes3 = !clip_key && !g.elems.is_empty();
            for e in g.elems.iter().take(3) {
                let h = src.get(e.off, 3)?;
                aes3 &= h.len() == 3 && aes3_count(e.len as usize, u16::from_le_bytes([h[1], h[2]])).is_some();
            }
            let pcm_ok = sd.coding[..8] == [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01] && sd.coding[8..12] != [0x04, 0x02, 0x02, 0x02] || sd.coding == [0; 16];
            let clip = clip_key;
            t.wrapping = if clip { Wrapping::Clip } else { Wrapping::Frame };
            let mut samples = Vec::new();
            let mut codec = Codec::Other;
            let mut fourcc = *b"mxfs";
            let mut aes3_fourcc = false;
            let mut out_channels = ch;
            if pcm_ok {
                let bits = (bytes * 8) as u16;
                if aes3 {
                    // 4-byte header (element data count u16 at 1..3), then the AES3 subframes of
                    // 32 bits, as many per sample as the element says
                    let mut sub = 0usize;
                    let mut sample_pos = 0i64;
                    for e in &g.elems {
                        if e.len < 8 || e.len > u32::MAX as u64 {
                            continue;
                        }
                        let head = src.get(e.off, 3)?;
                        let Some((n, s)) = aes3_count(e.len as usize, u16::from_le_bytes([head[1], head[2]])) else { continue };
                        if sub == 0 {
                            sub = s;
                        }
                        samples.push(Sample { offset: e.off, size: e.len as u32, dts: sample_pos, pts: sample_pos, key: true });
                        sample_pos += n as i64;
                    }
                    out_channels = ch.min(sub.max(1));
                    codec = Codec::Pcm { bits, float: false, big_endian: false };
                    aes3_fourcc = true;
                } else if clip {
                    let st = Stream::new(&g.elems);
                    let frame_bytes = (bytes * ch) as u64;
                    let total = st.total / frame_bytes;
                    let per = if t.edit_rate.0 > 0 && (t.edit_rate.0 as u64) <= 1000 * t.edit_rate.1.max(1) as u64 {
                        (sr as u64 * rate.1 / rate.0).max(1)
                    } else if let Some(vr) = video_rate {
                        (sr as u64 * vr.1.max(1) as u64 / vr.0.max(1) as u64).max(1)
                    } else {
                        1024
                    };
                    let mut at = 0u64;
                    while at < total && samples.len() < SAMPLES_MAX {
                        let n = per.min(total - at);
                        // a block may span two elements: cut it at the element end
                        let Some((off, run)) = st.at(at * frame_bytes) else { break };
                        let n = n.min(run / frame_bytes);
                        if n == 0 {
                            break;
                        }
                        samples.push(Sample { offset: off, size: (n * frame_bytes) as u32, dts: at as i64, pts: at as i64, key: true });
                        at += n;
                    }
                    codec = Codec::Pcm { bits, float: false, big_endian: false };
                } else {
                    let fb = (bytes * ch) as u64;
                    let mut at = 0i64;
                    for e in &g.elems {
                        let n = e.len / fb;
                        if n == 0 || e.len > u32::MAX as u64 {
                            continue;
                        }
                        samples.push(Sample { offset: e.off, size: (n * fb) as u32, dts: at, pts: at, key: true });
                        at += n as i64;
                    }
                    codec = Codec::Pcm { bits, float: false, big_endian: false };
                }
                fourcc = if aes3_fourcc { crate::mp4::AES3 } else { *b"lpcm" };
            } else {
                for (n, e) in g.elems.iter().enumerate() {
                    samples.push(Sample { offset: e.off, size: e.len.min(u32::MAX as u64) as u32, dts: n as i64, pts: n as i64, key: true });
                }
            }
            t.sound = Some(SoundDesc { channels: out_channels as u32, ..sd });
            (samples, codec, fourcc, sr)
        };
        t.samples = samples.len();
        if t.kind == Kind::Video && movie_timescale == 0 {
            movie_timescale = timescale;
            let last = samples.iter().map(|s| s.pts).max().unwrap_or(0);
            movie_duration = (last + rate.1 as i64).max(0) as u64;
        }
        let (width, height, par) = match &t.picture {
            Some(p) => picture_size(p),
            None => (0, 0, None),
        };
        let (channels, sample_rate) = match &t.sound {
            Some(s) => (s.channels as u16, timescale),
            None => (0, 0),
        };
        out_tracks.push(Track {
            storage: Default::default(),
            meta: Default::default(),
            id: t.track_id,
            kind: t.kind,
            fourcc,
            codec,
            timescale,
            width,
            height,
            rotation: 0,
            channels,
            sample_rate,
            samples,
            colour: None,
            default: true,
            codec_delay_ns: 0,
            discard_padding_ns: 0,
            par,
        });
    }
    if out_tracks.is_empty() {
        return Err(Error::Invalid("MXF: no essence found for the tracks"));
    }
    if movie_timescale == 0 {
        movie_timescale = out_tracks[0].timescale;
        movie_duration = out_tracks[0].samples.last().map_or(0, |s| (s.pts + 1).max(0) as u64);
    }
    // ordered like the tracks of the file package, with the unmatched ones left out
    let kept: Vec<MxfTrack> = tracks.into_iter().zip(assigned).filter(|(_, a)| a.is_some()).map(|(t, _)| t).collect();
    Ok(Mxf {
        run_in,
        partitions,
        pattern,
        essence_containers,
        primer,
        tracks: kept,
        index_segments: index.len(),
        index_entries: (have_entries, entries.iter().flatten().filter(|e| e.temporal_offset != 0).count(), entries.iter().flatten().filter(|e| e.flags & 0x80 != 0).count()),
        edit_unit_byte_count: ebc,
        file_package: res.file_package,
        material_package: res.material_package,
        related_packages: res.related,
        incomplete,
        movie: Movie { timescale: movie_timescale, duration: movie_duration, tracks: out_tracks },
    })
}

/// Coded size, and the pixel aspect ratio the descriptor's display aspect ratio implies.
fn picture_size(p: &PictureDesc) -> (u32, u32, Option<(u32, u32)>) {
    // the picture shown (display size), in frame lines when the fields are stored apart
    let w = p.display_width.filter(|&v| v > 0 && v <= p.stored_width).unwrap_or(p.stored_width);
    let fields = if p.frame_layout == 1 { 2 } else { 1 };
    let h = p.display_height.filter(|&v| v > 0 && v <= p.stored_height).unwrap_or(p.stored_height) * fields;
    let par = p.aspect.filter(|a| a.0 > 0 && a.1 > 0).and_then(|(an, ad)| {
        if w == 0 || h == 0 {
            return None;
        }
        let (n, d) = (an as u64 * h as u64, ad as u64 * w as u64);
        let g = gcd(n, d).max(1);
        Some(((n / g) as u32, (d / g) as u32))
    });
    (w, h, par)
}

fn video_codec<R: Read + Seek>(src: &mut Src<R>, pc: PictureCodec, pd: &PictureDesc, samples: &[Sample]) -> Result<(Codec, [u8; 4])> {
    let pcoding = pd.coding;
    Ok(match pc {
        PictureCodec::Avc | PictureCodec::Hevc => {
            // parameter sets from the first frames that carry them
            let mut found = None;
            let mut kind = pc;
            let mut acc: Vec<u8> = Vec::new();
            for s in samples.iter().take(64) {
                let d = src.get(s.offset, (s.size as usize).min(1 << 20))?;
                // the label says AVC for some HEVC writers: the first parameter set tells
                if kind == PictureCodec::Avc && annexb::nal_ranges(d).iter().any(|&(a, _)| annexb::hevc_type(&d[a..]) == 32 && d[a] & 0x81 == 0 && d.get(a + 1).is_some_and(|b| b & 7 == 1) && annexb::h264_type(&d[a..]) != 7) {
                    kind = PictureCodec::Hevc;
                }
                acc.extend_from_slice(d);
                let cfg = if kind == PictureCodec::Avc { annexb::avcc_from(&acc) } else { annexb::hvcc_from(&acc) };
                if cfg.is_some() {
                    found = cfg;
                    break;
                }
                if acc.len() > (4 << 20) {
                    break;
                }
            }
            match (found, kind) {
                (Some(config), PictureCodec::Avc) => (Codec::H264 { config }, *b"avc1"),
                (Some(config), _) => (Codec::Hevc { config }, *b"hvc1"),
                (None, PictureCodec::Avc) => (Codec::Other, *b"avc1"),
                (None, _) => (Codec::Other, *b"hvc1"),
            }
        }
        PictureCodec::Vc3 => {
            // the label's 14th byte is the compression ID less 1235; 1270 and up is DNxHR
            let cid = 1235 + pcoding[13] as u32;
            (Codec::Other, if cid >= 1270 { *b"AVdh" } else { *b"AVdn" })
        }
        PictureCodec::ProRes(p) => {
            let f = match p {
                1 => *b"apco",
                2 => *b"apcs",
                3 => *b"apcn",
                4 => *b"apch",
                5 => *b"ap4h",
                6 => *b"ap4x",
                _ => {
                    if pd.h_subsampling == 1 { *b"ap4h" } else { *b"apch" }
                }
            };
            (Codec::Other, f)
        }
        PictureCodec::Mpeg2 => (Codec::Other, *b"mpg2"),
        PictureCodec::Jpeg2000 => (Codec::Other, *b"mjp2"),
        PictureCodec::Uncompressed => (Codec::Other, *b"raw "),
        PictureCodec::Other => (Codec::Other, *b"mxfv"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ber_lengths() {
        assert_eq!(ber(&[0x7f]), Some((127, 1)));
        assert_eq!(ber(&[0x81, 0xc8]), Some((200, 2)));
        assert_eq!(ber(&[0x83, 1, 0, 0]), Some((65536, 4)));
        assert_eq!(ber(&[0x88, 0, 0, 0, 1, 0, 0, 0, 0]), Some((1 << 32, 9)));
        assert_eq!(ber(&[0x80]), None);
        assert_eq!(ber(&[0x83, 1]), None);
    }

    #[test]
    fn detection_with_and_without_run_in() {
        let mut pack = vec![0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x02, 0x04, 0x00];
        assert!(is_mxf(&pack));
        let mut run = vec![0xaa; 300];
        run.append(&mut pack);
        assert!(is_mxf(&run));
        assert!(!is_mxf(&[0, 0, 0, 0x18, b'f', b't', b'y', b'p', 0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02]));
        assert!(!is_mxf(&[0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!is_mxf(&[1, 2, 3]));
    }

    #[test]
    fn labels() {
        let atom = [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x10, 0, 0, 0];
        assert_eq!(OperationalPattern::from_label(&atom), OperationalPattern::Atom);
        let op1a = [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x01, 0x09, 0];
        assert!(OperationalPattern::from_label(&op1a).is_op1a());
        let vc3 = [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x0a, 0x04, 0x01, 0x02, 0x02, 0x71, 0x26, 0, 0];
        assert_eq!(classify_picture(&vc3, &[0; 16]), PictureCodec::Vc3);
        let prores = [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x0d, 0x04, 0x01, 0x02, 0x02, 0x03, 0x06, 0x04, 0];
        assert_eq!(classify_picture(&prores, &[0; 16]), PictureCodec::ProRes(4));
        let mpeg2 = [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x03, 0x04, 0x01, 0x02, 0x02, 0x01, 0x01, 0x11, 0];
        assert_eq!(classify_picture(&mpeg2, &[0; 16]), PictureCodec::Mpeg2);
        let avc_container = [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x0a, 0x0d, 0x01, 0x03, 0x01, 0x02, 0x10, 0x60, 0x01];
        assert_eq!(classify_picture(&mpeg2, &avc_container), PictureCodec::Avc);
    }

    #[test]
    fn aes3_to_plain_pcm() {
        // two samples, four subframes each, 24 bit: values 1, 2 (ch 0, 1) and the silent rest
        let sub = |v: i32, ch: u32| (((v & 0xff_ffff) as u32) << 4 | ch).to_le_bytes();
        let mut raw = vec![0, 2, 0, 0x0f];
        for s in 0..2 {
            for ch in 0..4u32 {
                raw.extend_from_slice(&sub(if ch < 2 { (s + 1) * 256 * (ch as i32 + 1) * if s == 1 { -1 } else { 1 } } else { 0 }, ch));
            }
        }
        assert_eq!(aes3_count(raw.len(), 2), Some((2, 4)));
        let mut out = Vec::new();
        aes3_to_pcm(&raw, 2, 24, &mut out);
        let v = |i: usize| i32::from_le_bytes([0, out[i * 3], out[i * 3 + 1], out[i * 3 + 2]]) >> 8;
        assert_eq!(out.len(), 12);
        assert_eq!([v(0), v(1), v(2), v(3)], [256, 512, -512, -1024]);
        let mut out16 = Vec::new();
        aes3_to_pcm(&raw, 2, 16, &mut out16);
        assert_eq!(i16::from_le_bytes([out16[0], out16[1]]), 1);
    }
}

/// Samples and subframes per sample of an AES3 element of `len` bytes whose header says `n`
/// samples; `None` when the size does not fit (the element is not in AES3 form).
fn aes3_count(len: usize, n: u16) -> Option<(usize, usize)> {
    let n = n as usize;
    if n == 0 || len < 8 || (len - 4) % (4 * n) != 0 {
        return None;
    }
    let sub = (len - 4) / (4 * n);
    (1..=16).contains(&sub).then_some((n, sub))
}

/// Reads the track tables of an MXF file.
pub fn read<R: Read + Seek>(r: &mut R) -> Result<Movie> {
    parse(r).map(|m| m.movie)
}

/// Turns one AES3 sound element (4-byte header, then 32-bit subframes: the 24-bit sample at bits
/// 4..28, `subframes` of them per sample) into interleaved little-endian PCM of `channels`
/// channels at `bits` (16 or 24) per sample, appended to `out`.
pub fn aes3_to_pcm(raw: &[u8], channels: usize, bits: u16, out: &mut Vec<u8>) {
    if raw.len() < 8 || channels == 0 {
        return;
    }
    let n = u16::from_le_bytes([raw[1], raw[2]]) as usize;
    let body = &raw[4..];
    let sub = if n > 0 { (body.len() / 4 / n).max(1) } else { channels };
    let keep = channels.min(sub);
    for frame in body.chunks_exact(4 * sub) {
        for c in 0..channels {
            let v = if c < keep { u32::from_le_bytes([frame[c * 4], frame[c * 4 + 1], frame[c * 4 + 2], frame[c * 4 + 3]]) } else { 0 };
            // 24-bit sample in bits 4..28
            let s24 = (v >> 4) & 0xff_ffff;
            if bits <= 16 {
                out.extend_from_slice(&((s24 >> 8) as u16).to_le_bytes());
            } else if bits <= 24 {
                out.extend_from_slice(&s24.to_le_bytes()[..3]);
            } else {
                out.extend_from_slice(&(s24 << 8).to_le_bytes());
            }
        }
    }
}
