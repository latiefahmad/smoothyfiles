# rustdecoder

smoothyfiles' own video engine, replacing FilmCraft in DirectLook and the preview pane.

## Why

DirectLook decoded video with FilmCraft, on the CPU. A 4K 59.94 fps HEVC Main 10 clip
(Sony XAVC HS, 77 Mbit/s) decoded at 42 frames a second on a Ryzen 7 9800X3D with nothing
else running, and stuttered under load. Media Foundation (the GPU's video engine) plays it
smoothly, so DirectLook routes videos past 1080p60 there for now (commit 32e9c58).
Media Foundation is a black box, though: no frame-exact seeking, no control over presentation,
and the MFPlay API it goes through is deprecated.

No CPU decoder plays 4K60 10-bit HEVC smoothly on ordinary machines. Fast players (mpv, VLC,
Movies & TV) hand the compressed data to the graphics card's video engine. rustdecoder does
the same, with every step around that call written here:

| Step | Who |
| --- | --- |
| Reading the container (MP4 / MOV, MKV / WebM, MPEG-TS / M2TS, MPEG-PS, AVI) | rustdecoder |
| Reading the container (MP4 / MOV, MKV / WebM, MXF) | rustdecoder |
| Splitting samples into NAL units, parsing VPS / SPS / PPS / slice headers | rustdecoder |
| Reference picture management (DPB, POC, RPS) | rustdecoder |
| Filling the DXVA picture parameters, slice controls, scaling lists | rustdecoder |
| Entropy decoding, inverse transform, prediction, filters | GPU video engine (D3D11 Video) |
| YUV → RGB, scaling, rotation, presentation (swap chain) | rustdecoder (shaders) |
| Timing, A/V sync, frame-exact seeking | rustdecoder |

Frames stay on the GPU from decode to screen: no copy back to system memory.

## Crate layout

`crates/rustdecoder`, no dependency on FilmCraft or rf-app.

- `mp4` — ISO BMFF reader. Box walk, `moov` → tracks: timescale, duration, rotation (`tkhd`
  matrix), sample description (`avcC`, `hvcC`, audio), sample table (`stts`, `ctts`, `stsc`,
  `stsz`/`stz2`, `stco`/`co64`, `stss`) flattened into one list: offset, size, decode time,
  presentation time, keyframe. Fragmented MP4 (`moof`) is read into the same table (phase 5).
- `mkv` — Matroska / WebM reader producing the same `Movie` / `Track` / flat sample table as
  `mp4` (see phase 5); `container::read` picks the reader from the first bytes.
- `ts`, `ps`, `avi` — MPEG transport stream (188 / 192 byte M2TS), program stream and AVI readers
  producing the same `Movie` (see "Containers: TS, PS, AVI" below); `container::read` detects
  them (sync bytes, `00 00 01 BA`, `RIFF....AVI `).
- `mxf` — MXF (SMPTE ST 377-1) OP1a and OP-Atom, same `Movie` / `Track` / flat sample table (see
  the MXF section below); `annexb` — Annex B byte stream to length-prefixed NAL units, `avcC` /
  `hvcC` made from the in-band parameter sets.
- `bits` — bit reader with Exp-Golomb and emulation-prevention removal.
- `h264`, `hevc` — parameter sets and slice headers.
- `av1`, `dxva_av1`, `av1dec` — AV1 OBUs, sequence and frame headers with per-slot state, `DXVA_PicParams_AV1` + tiles,
  `Av1Decoder` (phase 5).
- `vp9`, `dxva_vp9`, `vp9dec` — VP9 superframe split, uncompressed header, `DXVA_PicParams_VP9`,
  `Vp9Decoder` (phase 5).
- `mpeg2`, `dxva_mpeg2`, `mpeg2dec` — MPEG-1 / 2 video headers, picture splitter, `DXVA_PictureParameters` + slices, `Mpeg2Decoder` (MPEG-2 4:2:0 frame pictures on the GPU; see "MPEG-1 / MPEG-2 video on the GPU").
- `dnx`, `dnxtables`, `dnxdec` — Avid DNxHD / DNxHR (VC-3) on the CPU, `DnxDecoder` and `DnxVideo` (phase 6).
- `d3d11` — video device, decoder creation by profile (H.264, HEVC Main, HEVC Main 10, VP9 0 and 2, AV1 main),
  per-frame submission, output texture array.
- `present` — NV12 / P010 shaders, rotation, HDR → SDR tone mapping.
- `audio`, `resample`, `audio_out`, `playback` — PCM and AAC (OS decoder at first, own AAC later), sample-rate conversion, WASAPI output with the audio clock, A/V sync.

## Phases

1. MP4 reader with tests against ffprobe (sample count, sizes, timestamps, keyframes).
2. Bit reader, H.264 / HEVC parameter sets and slice headers.
3. D3D11 Video decode of HEVC Main / Main 10 and H.264 High, first frame to a PNG in a test.
   **Phase 3, HEVC done (2026-11-09):** `d3d11` (adapter pick by profile, decoder, texture
   array, submission), `slice` (slice header up to the long-term pictures; the GPU reads the
   rest), `dpb` (POC, RPS, marking, bumping, CRA/RASL skipping), `dxva` (PicParams, Qmatrix,
   short slice control), `decoder` (`HevcDecoder::open / next_frame / read_back`). Output is
   bit-exact with ffmpeg on Main and Main 10 clips with B-frames, open GOP pyramid, default
   scaling lists, 4 slices per picture, and on the first 60 frames of the Sony XAVC HS 4K60
   clip (7 slices, PPS scaling lists, leading RADL pictures). That clip decodes at 511 fps
   (330 frames, 0.65 s, GPU only, RTX 5070 Ti, short slice format, ConfigBitstreamRaw 1).
   Assumptions: short slice format only (a GPU offering only the long format is refused),
   4:2:0 8/10-bit, surfaces rounded up to 128, one frame stays valid until the next
   `next_frame`. Left for phase 4: frame queue holding several surfaces, shaders, seeking
   (start at a keyframe, drop RASL), mid-stream SPS changes beyond size, other GPUs.
   **Phase 3, H.264 done (2026-11-09):** progressive 4:2:0 8-bit Baseline, Main and High, CAVLC
   and CABAC, from MP4 / MOV (`avcC`). New: `h264` (SPS with VUI reorder / DPB limits and scaling
   lists, PPS, slice header up to dec_ref_pic_marking, `avcC` through `nal::avcc`), `h264_dpb`
   (POC types 0, 1 and 2, sliding window and MMCO 1 to 6, IDR / MMCO5 flush,
   `no_output_of_prior_pics`, bumping by the stream's own `max_num_reorder_frames` or else the
   level's DPB size), `dxva_h264` (`DXVA_PicParams_H264`, `DXVA_Qmatrix_H264`; the short slice
   structure is shared with HEVC), `h264dec` (`H264Decoder`, same shape as `HevcDecoder`), `video`
   (`VideoDecoder`: opens the file, picks the decoder from the track's codec, same methods).
   Findings: for H.264 `ConfigBitstreamRaw == 2` is the short slice format and 1 is the long one
   (the reverse of HEVC); sending short structures to the raw 1 configuration decodes nothing (a
   black surface, no error), so `HwDecoder::with_alignment` takes the value that means short
   and also requires the configuration without bitstream encryption. H.264 surfaces are rounded
   up to 16 (HEVC: 128). `DecoderBeginFrame` can answer `E_PENDING` when decoding runs far
   ahead of the GPU; it is retried like `DXGI_ERROR_WAS_STILL_DRAWING`. Unsupported, with
   a clear error: FMO (PPS), data partitioning,
   4:2:2 / 4:4:4, more than 8 bits, lossless transform bypass, frame_num gaps. A stream with
   `frame_mbs_only_flag` 0 whose pictures are all frames decodes. Streams starting on a non-IDR
   intra picture start there (earlier pictures dropped).
   Tests (`tests/gpu_h264.rs`, x264 clips 640x360, 3 s unless noted, compared sample by sample
   with `ffmpeg -f rawvideo` in display order): baseline CAVLC (POC type 2), main CABAC with
   B-pyramid (MMCO 1), main CAVLC, high 8x8 transform, high with `cqm=jvt` (PPS scaling lists),
   4 slices, `weightp=2:weightb=1` on a fade (explicit and implicit weights), 8 reference
   frames, 9 s long GOP (270 frames), open GOP (non-IDR I pictures), strict B-pyramid with
   MMCO, `fake-interlaced` (frame_mbs_only 0), 1080p level 4.1 phone style, and NVENC High
   with B-pyramid: all bit-exact. (Interlaced: see the next paragraph.) A 10 s 1080p60 High clip
   (600 frames, noisy) is bit-exact too and decodes at about 2000 fps (GPU only, no read-back,
   RTX 5070 Ti). MMCO 2 to 6, long-term pictures and POC type 1 have only unit tests in
   `h264_dpb` (x264 and NVENC never produce them).
   Left for H.264: frame_num gap concealment, High 10 and 4:2:2 profiles, the long slice
   format for GPUs offering only that.
   **H.264 interlaced done (2026-11-09):** MBAFF frames, field pictures (PAFF) and frame
   pictures in interlaced streams decode on the GPU, bit-exact against ffmpeg's woven frames.
   - MBAFF needed nothing but lifting the refusal: `MbaffFrameFlag` was already filled
     (`mb_adaptive_frame_field_flag` and not a field picture) and the hardware does the rest.
   - Field pictures: `h264_dpb::Pic` is a frame with two fields (`marks[2]`, `fields` mask, own
     top / bottom order counts). `start_picture` decides whether a field is the second field of
     the open frame (opposite parity, same `frame_num`, same reference-ness, not IDR) and says so
     in `Started::second_of`; `H264Decoder::decode` then decodes into the first field's surface.
     Output waits until the second field is decoded: a first field's frame is never bumped while
     open, and when the next picture is not its second field the lone field comes out as a frame
     (the other field's rows hold whatever the surface had). A non-reference first field that would
     go straight out is held until its frame is complete, so the buffer can briefly hold one
     more entry than `dpb_size` (the surface pool has 3 spare).
   - POC: types 0, 1, 2 per field (a bottom field in type 1 adds `offset_for_top_to_bottom_field`
     and uses `delta_pic_order_cnt[0]`); MMCO5 on a field zeroes that field's count; after MMCO5
     `prevPicOrderCntLsb` is 0 for a bottom field, the top count otherwise.
   - Marking per field: the sliding window counts a frame / pair / lone field once and does
     nothing for the second field of a pair whose first field is short-term; MMCO 1 to 3 resolve
     picNumX per field (`2 * FrameNumWrap + 1` same parity, `2 * FrameNumWrap` opposite;
     `CurrPicNum = 2 * frame_num + 1`) or per frame (both fields marked) in frame pictures;
     long-term indices are shared by the two fields of a frame.
   - DXVA: `CurrPic` carries the bottom field flag, `CurrFieldOrderCnt` only the picture's own
     parity, `FieldOrderCntList` and `UsedForReferenceFlags` per field (a field that is not a
     reference counts as absent, 0), `field_pic_flag` as before. The second field's reference list
     contains the first field's own entry (its surface is also `CurrPic`).
   - Tests (`tests/gpu_h264.rs`, 90 frames each, every sample compared, in MP4 unless noted):
     x264 `tff=1` and `bff=1` with B-frames (MBAFF; x264 writes no PAFF), NVENC with
     `-flags +ildct+ilme` and `setfield=tff|bff` (writes every picture as two fields: P fields
     only; with B fields, `-b_ref_mode middle`, 8 reference frames, non-reference B fields),
     the MBAFF and the PAFF B clip remuxed into MPEG-TS, and 1920x1080 MBAFF / PAFF in AVCHD style
     `.m2ts`: all bit-exact. NVENC clips skip where there is no NVENC. Unit tests in `h264_dpb`
     for pairing, output once per frame, a lone field, the sliding window over pairs and MMCO1 on a
     single field. Not covered by any stream here (unit tests or reasoning only): streams mixing
     field and frame pictures, long-term fields, MMCO 2 / 3 / 6 on fields, MMCO5 on a field, a
     reference first field with a non-reference second field (treated as two lone fields), fields
     of one frame in separate MP4 samples / PES (the pairing does not depend on it).
   - Display: the decoder gives the woven frame. `present::Converter::set_deinterlace(true)` blends
     each line with its two neighbours (1 2 1 on luma, the chroma row with the rows above and
     below) in the pixel shader: no comb on moving edges, a little less vertical sharpness; not
     motion adaptive and not a bob (no frame rate doubling). Off by default, and the player does
     not switch it on by itself yet (`StreamInfo` does not say a stream is interlaced).
     `tests/deinterlace.rs`: a 60 fps source woven into 30 interlaced frames, comb measure 1.50 woven,
     0.38 blended.
4. Playback in DirectLook: clock, frame queue, seeking, audio; replaces the Media Foundation
   route, and FilmCraft for video.
   **Phase 4 groundwork, headless core done (2026-11-09):**
   - `present` (`Converter`): NV12 / P010 slice → BGRA8 texture of any size on the decoder's
     device, one pixel shader compiled with `D3DCompile` when the converter is created (no
     shader tools in the build). Chroma upsampling is the sampler's bilinear filter on the half
     size chroma plane (centre siting), the matrix comes from the VUI (`Colorimetry::from_vui`:
     BT.709 / 601 / 2020 / SMPTE 240, unsignalled streams BT.709 when 1280 wide or more or over
     576 lines, else BT.601; limited or full range; 8 and 10 bit), rotation 0 / 90 / 180 / 270
     from the `tkhd` matrix, optional mirror, scaling by bilinear sampling of the visible
     rectangle (clamped half a sample inside it so the padding never bleeds in). HDR → SDR,
     **first version**: PQ and HLG are tone mapped with the BT.2390 EETF (1000 nit source peak
     assumed, no mastering display metadata read yet; target peak 203 nits = SDR white, black
     level 0, per channel), BT.2020 → BT.709 primaries, sRGB encoding. HLG goes through the
     1000 nit display OOTF (gamma 1.2) first. Per-channel mapping desaturates very bright
     colours a little, and 203 nit white lands at about 90 % (the curve's knee), by design of
     the first version. Timestamp queries give the GPU time per conversion.
   - `player` (`Player`): a worker thread owns the `HevcDecoder` and copies every output picture
     out of the decoder's array into a ring of 2..n slots (one texture array, same format);
     the ring bounds the decode-ahead (a slot is taken before decoding and returned when the
     picture is replaced). `Clock` (start / pause / resume / seek / time) over a `TimeSource`
     (`SystemTime`, `FakeTime` for tests). `frame_for(now)` shows the latest picture whose time
     has come and never one from the future; passed-over pictures count as dropped, pictures
     shown more than a frame late as late. `seek(t)` plans from the sample table
     (`plan_seek`): the picture with the largest pts not after `t` is the landing picture; decoding
     restarts at the sync sample before it, one sync sample further back so the RASL pictures
     of a CRA picture decode, and everything before the landing picture is dropped without a
     copy. Stats: decoded, skipped by seek, dropped, late, shown, most slots in use.
     D3D11's multithread protection is switched on for the device because the decoder thread
     (decode, copy) and the caller (convert) share the immediate context.
   - `HevcDecoder` got `gpu()`, `track()`, `seek_to_sample()`; `StreamInfo` got `rotation` and
     `vui`.
   - Checked: colour against ffmpeg (mean abs error per channel 0.04 to 0.5 of 255, see tests);
     rotation against ffmpeg's autorotate; PQ / HLG grey ramps against the curve worked out
     separately; queue order and repeats with a fake clock; ring bound; 31 seeks on an open GOP
     clip landing on the right picture with the same pixels as a linear decode. Camera clip
     (4K 59.94 fps Main 10, stored rotated 270): 5 s at real-time speed, decode + convert to
     1080x1920, 298..300 of 300 frames shown, 0..2 dropped, 0 late, 0.5 ms GPU per conversion
     (`examples/play_headless.rs`).
   - Left for phase 4: window / swap chain presentation in DirectLook (flip model swap chain,
     present the converted target or convert straight into the back buffer, vsync-aligned
     `frame_for`), audio (decode, output device, the audio clock as the master clock and A/V
     sync by dropping or repeating pictures against it), pause on a seek target, playback
     rate, looping, mid-stream size changes (the ring is built from the first picture), mastering
     display / content light metadata for the tone map, chroma siting other than centre,
     a better downscale filter than bilinear for large ratios, edit lists.
   **Phase 4, audio and A/V sync done (2026-11-09):**
   - `audio` (`AudioDecoder`): first audio track of an MP4 / MOV as interleaved `f32` blocks with
     their time in seconds. PCM (`lpcm`, `sowt`, `twos`, `in24`, `in32`, `fl32`, `fl64`, `ipcm`,
     `fpcm`; 8 / 16 / 24 / 32 bit int, 32 / 64 bit float, both byte orders) is converted here;
     AAC goes through Windows' `CMSAACDecMFT` (raw frames in, AudioSpecificConfig as
     `MF_MT_USER_DATA` after a 12 byte HEAACWAVEINFO tail, `MF_MT_AAC_PAYLOAD_TYPE` 0, float
     out). No FilmCraft, no ffmpeg at runtime. The decoder drops every frame before time 0 and
     before a seek target; the AAC encoder delay is the first edit of `elst`, which `mp4.rs`
     already applied (first packet at -1024), so the first sample returned is the sample at 0.
     A seek restarts AAC two packets before the target (priming) and trims.
   - `resample` (`Resampler`, `map_channels`): streaming windowed-sinc (Blackman-Harris, 16 taps
     per side, kernel stretched and cut off at 0.97 of the new Nyquist when going down, table of
     512 steps with linear interpolation), no latency to compensate. Channels: mono to the front
     pair, stereo to mono, 5.1 / 7.1 to stereo by ITU weights normalised so full scale cannot
     clip, others by common channels.
   - `audio_out` (`AudioOutput`, trait `AudioSink`): WASAPI shared mode, event driven, device mix
     format (float32; 16-bit PCM mix formats also handled), a render thread filling from a queue.
     The clock is `IAudioClock::GetPosition` interpolated with its QPC time (at most 20 ms,
     never backwards), with the silence written for under-runs taken out once played, so the
     clock stands still while the sound does; after `end_of_data` silence is time. It is a
     `TimeSource` (`AudioClockSource`): seconds of sound played, still when paused. Muting is
     explicit (`OutputOptions::muted`, the default): session volume 0, session mute, and silent
     buffers, all before the stream starts.
   - `playback` (`Playback`): `Player` plus an audio feed thread. The player's `Clock` runs over
     the sink's clock, so the picture shown is `frame_for` of the sound heard; the late / drop
     policy is the player's. `play` waits for 150 ms of queued sound before starting the device,
     `seek(t)` moves the video as before and the audio to the packet containing `t` with the
     sound before `t` dropped (the sink is flushed by the feed thread, so nothing old is
     queued after the call returns), pause / play keep both in step. No audio track, no device
     or an unsupported audio codec leave the system clock; `audio_note()` says why.
   - Checked (all on generated clips with ffmpeg as the reference, tests `audio`, `playback_av`,
     lib tests): PCM s16le / s24le in MOV and mono 44.1 kHz bit-exact with `ffmpeg -f f32le`
     (max diff 0, same length); AAC-LC stereo 48 kHz max abs diff 1.1e-3, RMS 2.5e-5, AAC mono
     44.1 kHz 1.5e-4 / 3.2e-6 (full scale 1.0), the first sample at 0.000 s and the length 144384
     / 133120 frames, identical to ffmpeg's (edit list handled); seeks land on the exact frame
     (PCM identical to a linear decode, AAC the same except the last 100 frames of the stream,
     where a second pass of the OS decoder rounds a little differently). Resampler: a 1 kHz sine
     keeps 1000.000 Hz and its level within 0.0002 dB for 44.1 to 48, 48 to 44.1, 96 to 48 and
     22.05 to 48 kHz; 30 kHz at 96 kHz to 48 kHz comes out below -60 dB. Sync with a fake audio
     clock: pictures in order and never from the future, none dropped at 60 Hz, audio blocks one
     unbroken run from 0, after seeks to 1.5 / 0.2 / 2.9 / 0 s the first audio block starts at
     the target (within 1/48000) and the first picture is the one at or just before it. Muted
     real device: clock advances 0.4929 s in 0.5032 s of wall time, 0 under-runs, 51 periods per
     half second; an under-run stops the clock (0.3 s queued, 0.9 s wall: 0.300 s).
   - Camera clip (Sony XAVC HS, `20261011_C5743.MP4`): audio is PCM `twos` (16-bit big endian)
     48 kHz stereo, 264264 frames; the device mixes at 48 kHz so nothing is resampled. 5 s muted,
     4K59.94 HEVC Main 10 converted to 1080x1920: clock advanced 4.991 s, 300 of 300 pictures
     shown, 0 dropped, 0 late, 0 audio under-runs, picture minus sound heard between -7.9 and
     0 ms (never more than half a frame; trend +0.5 ms per second, no creeping), longest wait
     between two pictures 25 ms.
   - Left for audio: HE-AAC / AAC-PS only written for, not tested (no encoder here), AC-3 / E-AC-3
     (done later for machines with a Dolby decoder, see phase 5), several audio tracks and track choice, 5.1 AAC channel order checked only
     by the downmix maths, `Player` is HEVC only (H.264 plays through `VideoDecoder` but not
     yet through `Player` / `Playback`), device change while playing (default device switched,
     `AUDCLNT_E_DEVICE_INVALIDATED`), exclusive mode / low latency, volume ramp on seek (a click
     is possible at a seek), playback rate (needs time stretching), edit lists beyond the first
     edit, AAC tail trim to the edit end, the system mixer's session volume persists per
     executable (muted runs of the test executables leave them muted).
5. MKV / WebM, VP9 and AV1 (the GPU decodes them too), fragmented MP4.
   **Phase 5, MKV / WebM, VP9 and AV1 done (2026-11-09); fragmented MP4 done below:**
   - `mkv` (`mkv::read`, `container::read`): EBML walk of the Segment: Info (`TimestampScale`,
     `Duration`), Tracks (`CodecID` `V_MPEG4/ISO/AVC`, `V_MPEGH/ISO/HEVC`, `V_VP9`, `V_AV1`,
     `A_AAC*`, `A_OPUS`, `A_VORBIS`, `A_PCM/INT/LIT|BIG`, `A_PCM/FLOAT/IEEE`; `CodecPrivate`,
     pixel size, `Colour`, `FlagDefault`, `DefaultDuration`, `CodecDelay`, audio rate / channels /
     bit depth), Clusters with SimpleBlock and BlockGroup (a group without `ReferenceBlock` is a
     sync sample), Xiph / EBML / fixed lacing (each laced frame is its own sample, timed by
     `DefaultDuration`, else `BlockDuration / n`), unknown-size Segment and Clusters (a cluster
     ends at the first element that cannot sit inside one), cut files (keeps what is complete).
     The result is the MP4 reader's `Movie`: `Track.samples` with offset, size, dts, pts, sync flag,
     so `HevcDecoder`, `H264Decoder`, `AudioDecoder`, `Player` and `Playback` read MKV through
     `container::read` unchanged. Only element headers and the bytes in front of each block are
     read (32 KiB window), one 32 byte `Sample` per frame stays in memory; payloads are never
     loaded. The `Cues` are skipped: every cluster is indexed, so the table already has every sync
     sample. A track's timescale is 1e9 / `TimestampScale` (1000), `pts` is the container's own
     integer (ffprobe prints the same). Audio times have `CodecDelay` taken off like ffmpeg does
     (Opus pre-skip, ffmpeg's AAC priming), so the first AAC packet starts before 0 as in MP4 and
     `AudioDecoder` trims it (it uses the exact delay for the first packet, not the rounded
     millisecond). PCM blocks follow each other: a start time rounded to the container's tick
     does not open gaps. Video `dts` are the `pts` sorted (Matroska has no decode time).
     Not handled, listed without samples: `ContentEncodings` (header stripping, compression,
     encryption: the frames are not a plain offset plus size). Video rotation (`Projection`) is not
     read yet. 8-bit PCM is read as signed like QuickTime's `twos`; Matroska's is unsigned.
   - Checked against ffprobe (`-show_packets`: count, size, pts, keyframe flag, all equal, every
     packet): H.264 + AAC mkv (90 + 142 packets), HEVC + PCM mkv (90 + 141), VP9 + Opus webm
     (90 + 151), VP9 + Vorbis webm (90 + 143), `-cluster_size_limit 30000 -cluster_time_limit 300
     -reserve_index_space 4000` (14 clusters), `-live 1` (unknown-size Segment, no Cues) for mkv
     and webm; `Colour` (BT.2020 / PQ / limited) read back. NAL lengths of every 7th H.264 / HEVC
     sample add up to the sample, VP9 frame markers of all samples are right (the offsets point at
     the frames). ffmpeg's muxer never laces and never writes unknown-size clusters, so a hand
     built file covers Xiph, EBML and fixed lacing (3 + 4 + 5 frames), block groups with and
     without references, an unknown-size Cluster followed by a sized one, Void elements and a
     cut file; ffprobe reads the hand built file with the same audio packets (pts, size).
   - `vp9` + `dxva_vp9` + `vp9dec` (`Vp9Decoder`, `VideoDecoder::Vp9`): `split_superframe`
     (the index at the end of a packet; hidden frames travel with the next shown one), the
     uncompressed header (frame type, `show_existing_frame`, `intra_only`, reset / refresh flags,
     reference indices and sign bias, size from a reference, interpolation filter, loop filter and
     its delta persistence, quantizer, segmentation with its persistence, tiles, compressed
     header size, colour config: profiles 0 to 3, 8 / 10 / 12 bit parsed), `Parser` keeps what a
     header depends on (deltas, features, colour, size of each of the 8 slots). `DXVA_PicParams_VP9`
     (208 bytes packed, laid out from the SDK's `dxva.h`; `frame_refs` and `ref_frame_map` hold
     surface indices like ffmpeg's `dxva2_vp9.c`; `use_prev_in_find_mv_refs` is: not error
     resilient, same size as the last decoded frame, and that frame was shown),
     `DXVA_Slice_VPx_Short` (one per frame, the whole frame is the bitstream: the video engine reads
     the compressed header and the tiles), no scaling lists. Short slice format is
     `ConfigBitstreamRaw` 1 as for HEVC. The host keeps 8 slots and a use count per surface (a pool
     of 12, surfaces 64 aligned); frames go out in decode order (VP9 does not reorder);
     `show_existing_frame` queues the slot's surface again. Profile 0 (NV12) and profile 2, 10 bit
     (P010). Refused with a clear error: profiles 1 and 3 (4:4:4 / 4:2:2 / 4:4:0), 12 bit, a bit
     depth change inside the stream, an inter frame larger than the surfaces made for the first
     key frame. A key frame of another size is fine: smaller keeps the surfaces, larger makes a
     new decoder (frames carry their own size).
   - Bit-exact against ffmpeg (libavcodec's VP9 decoder, `-f rawvideo`, every sample of every
     shown frame, `-noautoscale -fps_mode passthrough`), libvpx-vp9 clips 640x360 unless noted,
     3 s, 90 frames: defaults; 1-pass with `-lag-in-frames 25 -auto-alt-ref 1` (libvpx makes no
     hidden frames 1-pass); no alt ref, GOP 30; 1280x720 with `-tile-columns 2`; `-tile-columns 1
     -tile-rows 2`; `-error-resilient 1 -frame-parallel 1`; `-aq-mode 1` and `-aq-mode 3`
     (segmentation on 90 / 89 frames); `-lossless 1`; 642x362 (odd size); `-sharpness 5
     -static-thresh 200`; 2-pass `-auto-alt-ref 1` (8 superframes with hidden frames); 2-pass
     `-auto-alt-ref 6 -enable-tpl 1` on a clean source (20 superframes, 42 hidden frames, 42
     `show_existing_frame`); profile 2 10-bit, 1-pass and 2-pass multi-layer alt ref (17
     superframes, 36 hidden, 36 show-existing); a 640x360 stream joined to a 320x180 one at a key
     frame and the other way round (120 frames, both bit-exact); a 1080p60 8 Mbit clip of 300
     frames. Also H.264 and HEVC in mkv (B-frames, 90 frames each). All identical.
     libvpx in this ffmpeg has no resize mode, so inter frames with references of another size
     (scaled references), and `intra_only` frames (libvpx makes none here) are not exercised;
     both are written to the spec and the DXVA structure carries the reference sizes.
   - Speed (RTX 5070 Ti, GPU only, no read-back): 1080p60 VP9 8 Mbit, 300 frames in 0.20 s,
     about 1500 fps (HEVC / H.264 1080p: 500 to 2000 fps, same order).
   - `Playback::open` works on MKV / WebM with H.264, HEVC or VP9: the player and the sync
     tests (pictures in order next to the audio clock, seeks landing both sides on the target
     within 1/48000 s) pass on mkv with H.264 + AAC, HEVC + PCM, VP9 + PCM and VP9 + AAC (and,
     since the audio codecs below, VP9 + Opus and VP9 + Vorbis WebM, H.264 + FLAC and + MP3 MKV).
     Matroska times are whole milliseconds, so a 30 fps picture can be 0.7 ms off its true
     time (worst lag 33.7 ms instead of 33.3 ms at 60 Hz).
   - **AV1 (2026-11-09, `av1`, `dxva_av1`, `av1dec`, `VideoDecoder::Av1`, MP4 `av01` / `av1C` and
     Matroska `V_AV1`):** `av1` splits temporal units into OBUs (`obu_has_size_field` or the rest
     of the packet; the operating point 0 layer filter), reads the sequence header (profile, bit
     depth, colour, order hint bits, all `enable_*`, film grain present, the decoder model fields
     that shift the bit position) and the whole uncompressed frame header: frame type,
     `show_existing_frame`, refresh flags, reference indices (also `frame_refs_short_signaling`),
     frame size with refs, superres, tile info (uniform and explicit), quantization, segmentation,
     delta q / lf, loop filter, CDEF, loop restoration, tx mode, reference select, skip mode,
     global motion (sub-exponential codes against the previous parameters), film grain (also
     `update_grain = 0`). Per slot it keeps what a refreshing frame stores (order hints, sizes,
     global motion, loop filter deltas, segmentation features, film grain parameters); CDFs and
     motion vectors stay in the driver's per-surface state. `parse_tile_group` gives each tile's
     offset and size; the frame is submitted when its last tile group arrives: one bitstream
     buffer with the tile data of all groups, one `DXVA_Tile_AV1` (16 bytes) per tile, one
     `DXVA_PicParams_AV1` (912 bytes, layout asserted in a unit test). The GPU applies film
     grain itself (`film_grain.apply_grain` in the structure). Profile
     `D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0`, NV12 / P010, `ConfigBitstreamRaw` 1, surfaces 64
     aligned, 12 surfaces; pictures go out in decode order like VP9 (hidden frames are shown by
     `show_existing_frame`; a shown key frame refreshes all slots).
   - Lessons (RTX 5070 Ti, driver as of today): a non-zero `StatusReportFeedbackNumber` crashed
     the whole process (`STATUS_STACK_BUFFER_OVERRUN` in the driver) exactly on the 25th frame,
     for any stream, whatever the value; zero is used. `frame_refs[i].width / height` must be the
     size the reference was *coded* at (before superres upscaling); `PicParams.width` is the
     upscaled width and `superres_denom` is 8 without superres (value 0 also decoded). With the
     upscaled size for the references every inter frame of a superres stream differed from
     dav1d's by a few levels. `frame_restoration_type` is the spec's enum after the
     `Remap_Lr_Type` step (0 none, 1 Wiener, 2 SGR, 3 switchable), CDEF secondary strengths are
     the raw 2-bit values, `qm_*` is 0xff without quantizer matrices, the film grain AR
     coefficients are the "plus 128" bytes.
   - Bit-exact against libdav1d through ffmpeg (`-f rawvideo`, every sample of every shown
     frame, `tests/gpu_av1.rs`, clips made by ffmpeg into the shared temp folder), 90 frames each
     unless noted, no mismatch in any:
     SVT-AV1 8-bit MP4 and WebM (hidden frames, 43 `show_existing_frame`),
     SVT-AV1 10-bit MP4 and WebM, SVT-AV1 1280x720 with 4x2 tiles
     (loop restoration on in 40 frames), SVT-AV1 film grain 8 (`apply_grain` in all 90 frames,
     43 of them shown by `show_existing_frame`), SVT-AV1 superres (fixed denominator 12, and
     random per frame), SVT-AV1 random frame sizes (reference scaling, 16 different odd and even
     sizes, 427x240 ... 640x360), libaom (defaults with lag, 642x362 with 2x2 tiles in WebM,
     `aq-mode=1` segmentation, quantizer matrices, delta q + delta lf, error resilient, lossless,
     screen content, 128x128 superblocks with 2 tile groups), rav1e. A seek to a later key
     frame gives the same pictures as decoding from the start.
   - Speed: 1080p AV1 (SVT-AV1 6 Mbit, 180 frames), GPU only, no read-back: about 1360 fps on the
     RTX 5070 Ti (noisy test pattern, so the stream is not typical, but the order is that of VP9).
   - Not tested or not done for AV1: 4:4:4 / 4:2:2 (profile 1, 2) and monochrome (refused with a
     message), 12 bit, operating points above 0 (only the layer filter is written), large scale
     tile, switch frames and `frame_id_numbers_present` streams (parsed per spec, no clip),
     `frame_refs_short_signaling` (written to the spec, no encoder here uses it), intrabc
     (libaom screen content may not have used it), multiple video decoders at once with AV1,
     Opus / Vorbis audio, the player and `Playback` on AV1 files (the `Video` path is shared,
     but no sync test has run on an AV1 clip), the rf-app side (`rd_video.rs` probe, `DirectLook`
     route).
   - Left for phase 5: (AV1 is done, see above),
     Opus and Vorbis decoding (own, like the rest)

   **Fragmented MP4, done (2026-11-09):** `mp4::read` walks the top-level boxes as before (`mdat`
     is skipped, never read); every `moof` after `moov` is read whole (a few hundred bytes) and
     its samples are appended to the track's table after the ones `moov` lists, so the decoders,
     `Player` and `Playback` see one flat table and nothing downstream changed. `sidx`, `mfra` and
     `tfra` are not needed (all `moof`s are walked).
   - Parsed: `mvex` / `trex` defaults; `mfhd`; per `traf`: `tfhd` (base-data-offset,
     sample-description-index, default duration / size / flags, default-base-is-moof),
     `tfdt` (v0 / v1, a negative decode time written as unsigned is read back signed),
     any number of `trun` (data offset, first-sample-flags, per-sample duration / size / flags /
     composition offset, v0 and v1 signed). The data base is the named one, else the `moof`
     (default-base-is-moof, and the first `traf` otherwise), else the end of the previous `traf`'s
     data; a `trun` without a data offset follows the previous `trun`. A sync sample is one
     whose flags have neither `sample_is_non_sync_sample` nor `sample_depends_on = 1`
     (ffmpeg's rule). Without `tfdt` the times go on from the last sample. The first edit
     of `moov`'s `elst` shifts fragment samples the same way as the `moov` ones. A track whose
     tables are missing (a fragmented file may omit `stco` and friends) starts empty. A `traf`
     for an unknown track is skipped. `Movie::duration` is worked out from the samples when
     `mvhd` says 0 (`empty_moov`).
   - Mixed files (`moov` has samples, fragments follow) and `empty_moov` files give one table. A
     broken `moof` stops the walk (what follows would be timed from a wrong place); a `moof`
     cut off at the end of the file is dropped, and samples of the last `mdat` that are not
     entirely in the file are dropped (ffprobe hands out the partial last packet, we do not; a
     partial picture cannot be decoded).
   - Cost: only box headers and the `moof` boxes are read. A one hour 320x180 30 fps clip with
     AAC (382 MB, 3600 fragments, 108 000 + 168 751 samples) opens in 25 ms (warm file cache).
   - Checked against ffprobe (`-show_packets`: count, size, file position, pts, dts, keyframe flag
     of every packet, video and audio, H.264 with B-frames + AAC and HEVC + AAC, 90 video + 142
     audio packets each, 6 keyframes): `frag_keyframe+empty_moov`, `+default_base_moof`,
     `empty_moov -frag_duration 500000` (time based fragments), `+dash`,
     `+separate_moof`, `+cmaf`, `+negative_cts_offsets`, `empty_moov+default_base_moof
     -frag_duration 1` (a fragment per frame), mixed `frag_keyframe` (samples in `moov` plus
     fragments): all equal. Cut copies (middle of the last `mdat`, middle of the last `moof`, a
     cut mixed file): equal to ffprobe minus its partial last packet (82 + 116 packets for the
     cut `mdat`). Hand built file (ffmpeg never writes these): base-data-offset, `tfhd` and
     `trex` defaults, two `trun`s in a `traf`, first-sample-flags, per-sample flags, v1 signed
     offsets, no `tfdt`, an unknown track, cut `moof` and cut `mdat`.
   - Two places where the timeline differs from ffprobe's, both with negative composition
     offsets (CMAF): ffmpeg moves the presentation times up by the most negative offset in
     fragments (our pts are decode time + offset as the spec says: first picture at 0, ffprobe
     says 1024 / 15360 s), and for `moov` tables followed by fragments it moves the `moov` part's
     decode times down but not the fragments' (its dts jump at the boundary, ours run on). Sizes,
     positions, keyframes and spacing are equal; the tests allow exactly those constant shifts
     for those clips only.
   - Decoding: the GPU decoders give bit-exact pictures against ffmpeg (`-fps_mode passthrough`,
     NV12, every sample of every frame) on 90 of 90 frames for H.264 empty_moov, mixed, CMAF,
     a fragment per frame, HEVC empty_moov, separate_moof, CMAF, and on the 80 whole pictures of
     the cut recording. `Playback` with a fake clock and AAC (60 Hz, 2.5 s, 76 pictures, none
     dropped, audio clock equal to the movie clock, one unbroken run of audio from 0, four seeks
     with audio restarting exactly on the target): H.264 empty_moov (CMAF offsets), mixed,
     HEVC CMAF, H.264 CMAF.
   - Left: a fragmented file with B-frames and plain (non-negative) composition offsets, as
     ffmpeg writes by default, has no edit list, so its first picture is at 1024 / 15360 s while
     the AAC starts at 0 (ffprobe says the same: start 0.0667 s against 0). `Playback` starts the
     clock at the first picture and the sound from its own start, so the sound plays about 67 ms
     late against the picture (the sync tests on such a file fail on that; they use CMAF clips
     for now). The clock should start at the earlier of the two streams, or the sound start at
     the clock like after a seek. Not handled: a change of sample description index inside a
     file (a track with two codecs), `sidx`-only random access (not needed), `senc` / `pssh`
     (encrypted fragments: the samples are listed, the data is not decryptable), `emsg`, and
     reading fragments that arrive while a file is still growing (the reader is one pass).

   **Audio codecs beyond AAC, done (2026-11-09):** `audio.rs` generalises the AAC path into
   `AudioMft` (any Media Foundation audio decoder found with `MFTEnumEx(MFT_CATEGORY_AUDIO_DECODER,
   input subtype)`, packets with time and duration in, `f32` out, 16/24/32-bit PCM converted),
   `MftCodec` names the codec, its seek preroll (packets decoded and dropped before a seek
   target: AAC / AC-3 2, MP3 4, Opus 4 = 80 ms) and the note when no decoder exists
   (`audio_note()`: "audio track not playable: not supported yet: AC-3 audio: no decoder
   installed (...)"); playback goes on without sound. `cargo run --example audio_mfts` lists what
   a machine has, `--example audio_dump <file>` decodes a track and prints blocks. Containers:
   MP4 `Opus`+`dOps` (turned into an `OpusHead`), `fLaC`+`dfLa`, `.mp3` / mp4a object type 0x6B / 0x69,
   `ac-3`+`dac3`, `ec-3`+`dec3`; Matroska `A_OPUS`, `A_FLAC`, `A_MPEG/L3`, `A_AC3`, `A_EAC3`,
   `A_VORBIS`, and `DiscardPadding` (`Track::discard_padding_ns`).
   - Measured against `ffmpeg -f f32le` of the same track (tests `audio_codecs`, 3 s stereo
     48 kHz sine clips; "shift" = frames our output is ahead of ffmpeg's after finding the best
     alignment, 0 = same sample at the same time):

     | codec | decoder | MKV / WebM | MP4 |
     |---|---|---|---|
     | FLAC (16 bit stereo 48 k, mono 44.1 k) | own `flac.rs` | bit exact (max diff 0), 144000 / 132300 frames, first sample 0 | bit exact, same |
     | Opus | Microsoft Opus Audio Decoder MFT | max diff 3.7e-8, rms 7.9e-9, 144000 frames = ffmpeg's, shift 0 | max diff 3.4e-5, rms 7.2e-7, 144648 frames = ffmpeg's, shift 0 |
     | MP3 (libmp3lame 128k) | MP3 Decoder MFT | max diff 3.0e-5, rms 1.6e-5, shift -528 | same |
     | Vorbis (libvorbis) | FFmpegAudioDecoder MFT (Web Media Extensions) | content identical (max diff 0), shift -128, 144832 frames against 144000 | not valid in MP4 |
     | AC-3, E-AC-3 | none on this machine | not decoded, note says why | same |

   - Opus: the transform would cut the pre-skip itself, which breaks seeking, so it gets an
     `OpusHead` with pre-skip 0 and the decoder trims by time like AAC (Matroska `CodecDelay`
     exact for the first packet, MP4 `elst` media time). The end is cut by `DiscardPadding` of the
     last block (packets are taken to be as long as the first, the last by its TOC): 144000
     frames in MKV / WebM as ffmpeg; ffmpeg does not cut the end in MP4 (144648) and neither do
     we. Seeks (`playback_av`) land on the target to 1/48000 s with Opus, Vorbis, FLAC and MP3.
   - FLAC: Windows' FLAC transform (`Microsoft FLAC Audio Decoder MFT`) accepts the type but
     `ProcessOutput` blocks for good after one or two frames (tried: every `MF_MT_USER_DATA`
     form, with / without the `fLaC` header as a first sample, sample times and durations, small
     and large output buffers, with and without the stream messages). Decoded here instead (~330
     lines, from the format description: all subframe types, stereo decorrelation, wasted bits,
     8-32 bit); no CRC check. A blocked transform cannot be used with a timeout, so no fallback.
   - MP3: the transform removes its own decoder delay (528 frames) which ffmpeg leaves in when the
     container has no LAME / Xing tag: our sound is 528 frames (11 ms) earlier in content, the
     length 143519 against ffmpeg's 144000 (MKV) / 144047 (MP4). This is more correct
     (the tone's phase starts at 0), not a bug; gapless playback by the LAME tag is not done.
   - Vorbis: the transform's first output starts 128 frames (blocksize_0 / 2) into the signal, so the
     first 2.7 ms of the fade-in are missing and the sound is 128 frames early; the tail is 832
     frames (17 ms) longer than ffmpeg's (no end trim). Not corrected (rule not verified for other
     block sizes: ffmpeg's native encoder clip differs again by 256 frames in length).
   - AC-3 / E-AC-3: only "Dolby AC-3 and E-AC-3 IEC-61937 converter MFT" is registered here (it
     wraps the stream for S/PDIF, no PCM out). The Dolby Digital (Plus) decoder comes with the
     Dolby Access store app / some editions, so nothing was decoded or measured for 5.1; the code
     path (any transform offering float / PCM output) is there and the tests skip with the note.
     An own AC-3 decoder would be the way to cover every machine.
   - Left for phase 5: AV1 (`Codec::Av1` is recognised, no decoder: `D3D11_DECODER_PROFILE_AV1_VLD_*`,
     OBU parsing, film grain), own Opus / Vorbis / MP3 / AC-3 decoders if the OS ones turn out
     missing on users' machines (Opus MFT is Windows 10 1809+, Vorbis needs the Web Media
     Extensions, AC-3 has no decoder here), several audio tracks,
     `ContentEncodings` header stripping (mkvmerge's default for some codecs), Matroska
     `Projection` rotation and HDR metadata into the tone mapper, VP9 4:4:4 (profile 1 / 3), 12 bit,
     scaled references and `intra_only` tests with a stream that has them, several video tracks
     (the default-flagged track is chosen), the `probe` in rf-app's `rd_video.rs` (still MP4 / MOV /
     M4V only; route `.mkv` / `.webm` through `container::read` there).
6. Formats no GPU decodes, on the CPU: ProRes and DNxHR (intra-only, slices decode in
   parallel), MXF, AIFF. Editing sources must keep previewing.
   - ProRes done (`prores` + `proresdec`, `VideoDecoder::ProRes`), written from the public
     description of the format (SMPTE RDD 36), no code taken from anywhere. `prores`: frame header
     (size, 4:2:2 / 4:4:4, interlace mode, alpha type, the two quantisation matrices, colour
     code points), picture headers (one per field), slice table, slices (a row of 1 / 2 / 4 / 8
     macroblocks): the DC coefficients of a plane coded as differences, the AC coefficients as
     (run, level) pairs walked block by block through the slice, both with the adaptive
     Rice / Exp-Golomb hybrid codes (the codebook follows the last values), dequantisation with
     the slice's scale and the frame's matrices, an integer 8x8 IDCT, clipping to 4..max-4.
     Output is planar `u16`: 10 bit for Proxy / LT / Standard / HQ (`apco apcs apcn apch`), 12 bit for
     4444 / 4444 XQ (`ap4h ap4x`). Interlaced frames: both fields woven into the frame (the
     scan order of interlaced frames differs). Slices write disjoint macroblocks, so a rayon
     pool (`ProResDecoder::new(threads)`, 1 = calling thread) decodes them in parallel; the result
     is identical for any thread count (tested).
   - On the GPU side: the picture goes up as one R16_UNORM texture per picture (Y on top, then U
     and V side by side for 4:2:2 / below one another for 4:4:4, samples moved to the top bits
     like P010) into a 2 slice array; `Converter` got a second pixel shader (`ps_planar`) that
     reads it, chroma upsampled by the sampler (same siting as the NV12 path), then the shared
     colour code. The NV12 / P010 path is untouched (same shader function, `shade`). `Player`'s
     ring takes the texture as it is (a GPU copy into the slot), `Playback` and its sync tests
     work unchanged. `Gpu::open_any` makes the device without asking for a decoder profile and
     shares the idle pool. MOV `apch ...` sample entries stay `Codec::Other` with the fourcc in
     `Track::fourcc`; `VideoDecoder::open` recognises them by it.
   - Exactness against ffmpeg's decoder (`-f rawvideo`, every sample, clips made by `prores_ks`
     and `prores_aw`): entropy decoding, scans, block order, dequantisation, clipping are bit
     exact; the IDCT is an accurate integer one (constants 2^16 sqrt2 cos(k pi / 16), rows >> 17,
     columns >> 20 at 10 bit / >> 18 at 12 bit, round half up) and differs from ffmpeg's own by 1 in
     the last bit for some samples: 422 10 bit 0.01 to 0.08 % of the samples (1080p: 0.011 to
     0.017 %), 4444 12 bit 1.5 to 4 % (the same absolute error on a 4 times finer scale). Never
     more than 1. Searching constants (14, 16, 18 bit), shifts and roundings (4 families, 300
     variants) did not reach 0: ffmpeg's integer IDCT has its own rounding noise; RDD 36 allows
     such a difference. Table in the test output (`cargo test -p rustdecoder --release --test
     prores -- --nocapture`).
   - Speed, Ryzen 7 9800X3D, noisy (so heavy) 422 HQ clips, decode only, slices on 1 / 4 / 8 /
     16 threads: 1080p (0.97 MB per frame, 193 Mbit/s at 25 fps) 50 / 202 / 319 / 409 fps; 2160p
     (3.83 MB, 766 Mbit/s) 12 / 44 / 84 / 122 fps; 1080p 4444 12 bit (1.44 MB) 31 / 116 / 207 /
     255 fps. With the packing and the upload to the GPU (`VideoDecoder::next_frame`, the
     player's pool is 8 threads): 1080p 187 fps, 2160p 43 fps. So 4K25 previews in real time
     with room to spare, 4K50 with the pool as it is.
   - Tests: 4 profiles at 640x360 and 1080p, `prores_aw`, odd sizes (650x366, 322x178: partial
     macroblocks and narrower last slices), 4444 with and without alpha, 4444 XQ, interlaced top
     and bottom field first (640x360, 1080p), damaged frames (400 cuts / flips / header hits
     each on 422 and 4444: no panic), the GPU picture against ffmpeg's RGB (mean abs error
     0.004 of 255), the player (order, slow polling, ring, 31 seeks with pixel checks, seek
     and play on) and `Playback` with PCM (pictures follow the audio clock, seeks land both
     sides on the target).
   - Left: the alpha plane of 4444 is stepped over, not decoded (no alpha path in the
     converter); 4444 12 bit exactness is within 1, not exact; a 4:4:4 file flagged 10 bit
     (`ap4h` is always read as 12 bit, like ffmpeg); a clip that changes size inside the
     stream is refused; ProRes RAW is another codec and not read; ProRes in Matroska / AVI
     is not looked for.
   - DNxHD / DNxHR done (`dnx` + `dnxtables` + `dnxdec`, `VideoDecoder::Dnx`, MOV `AVdn` / `AVdh`;
     ffmpeg writes `AVdh`), the same shape as ProRes. Written from SMPTE ST 2019-1:2016, which
     SMPTE publishes as a PDF (`pub.smpte.org/pub/st2019-1/st2019-1-2016.pdf`); no ffmpeg
     source read. `dnxtables.rs` was generated from the standard's own tables (Annex D weights,
     Annex E codeword tables, extracted from the PDF by a throw-away script; every table is a
     complete prefix code, which the unit test checks). `dnx`: coding unit header (HS, HVN,
     picture size, bit depth, scan type, CID, sub-sampling, colour format, macroblock row count
     and the row index table), field coded interlaced frames (two coding units, first field
     on the even lines), per macroblock row: 12 bit macroblock header (qsf and ACF), 8 blocks (4:2:2) or 12
     (4:4:4) per macroblock, DC as a bit count + bits (difference from the previous block of the
     component, reset at each row), AC as amplitude codeword + sign + optional amplitude index +
     optional zero run, end of block code; dequantisation with qsf and the CID weights; the
     ProRes integer IDCT (`prores::idct`, reused); output planar `u16`, 8 / 10 / 12 bit. CIDs:
     1235 1237 1238 1241 1242 1243 1244 1250 1251 1252 1253 1258 1259 (DNxHD, 1080p / 1080i /
     720p, 8 and 10 bit; 1238 1243 1244 1252 1258 1259 are from the standard's tables only, ffmpeg's
     encoder does not write them, so they are untested), 1270 (DNxHR 444), 1271 (HQX), 1272 (HQ), 1273 (SQ), 1274 (LB). Rows
     are independent, so a rayon pool decodes them in parallel (`DnxDecoder::new(threads)`);
     result identical for any thread count (tested).
   - Where ffmpeg (and so, in practice, the files) deviates from the standard's text, the code
     follows the files. Found with hand made frames (a script writes a coding unit with one
     coefficient of a known level per block using the tables above, ffmpeg decodes it, a
     forward DCT of its output gives the coefficient it used): (1) the reconstruction point
     of level `a` is `(a + 3/4)` steps (`((4a + 3) W qsf + 2p) / 4p`), the standard's eq. 8.1 has `(a + 1/2)`;
     with `(a + 1/2)` the error against ffmpeg is up to 14 at LB, with 3/4 it is 1; (2) CID 1271
     takes the weights of Table D.4, not D.1 (the standard's C.2 says D.1; the measured weights
     of ffmpeg's 1271 are D.4 to within the measurement noise). The same frames showed 12 bit
     samples come out 4 times larger than the ortho-normal DCT of the same coefficients (the
     DC of 64 is +8 at 10 bit and +32 at 12 bit): applied as an IDCT scale of 4 at 12 bit. A CID
     1235 / 1241 / 1250 stream uses p = 8, the rest p = 32 (eq. 8.1), as the standard says;
     ffmpeg agrees on all of them.
   - Exactness against ffmpeg's decoder (`-f rawvideo`, every sample): entropy decoding, scans,
     block order, DC prediction, field weaving are right (no error above the IDCT rounding);
     the whole decode is within 1 of ffmpeg for 4:2:2 (1.3 to 6.3 % of the samples differ by
     exactly 1: 1080p 1.3 to 2.0 %, 640x360 2.4 to 3.7 %, small odd sizes up to 6 %), within 2 for
     4:4:4 10 bit (2.7 to 7.3 % differ), and within 6 / 9 for 12 bit hand-patched frames (4
     times finer scale; mean error 0.12 to 0.15). A float IDCT instead of ours lowers the 1 off
     rate (3.7 to 2.0 % at LB 640x360), so most of it is ffmpeg's integer IDCT, not ours; the
     part left is the 3/4 rule, which is a measured law, not a proven one. Table in the test
     output (`cargo test -p rustdecoder --release --test dnx -- --nocapture`).
   - Speed, Ryzen 7 9800X3D, decode only, rows on 1 / 4 / 8 / 16 threads: 1080p DNxHR HQ 8 bit
     (0.92 MB per frame, 184 Mbit/s at 25 fps) 48 / 180 / 342 / 527 fps; 2160p HQX 10 bit (3.64 MB,
     728 Mbit/s) 13 / 49 / 92 / 143 fps; 1080p 444 10 bit (1.84 MB, 367 Mbit/s) 31 / 114 / 217 /
     342 fps. With packing and upload (`VideoDecoder::next_frame`): 1080p HQ 165 fps, 2160p HQX
     46 fps.
   - Tests: LB / SQ / HQ at 640x360 and 1080p, HQX 10 bit, 444 10 bit, odd sizes (650x366, 322x178,
     1001x563, 333x211), DNxHD at fixed bit rates (1080p 8 bit 120M and 36M, 10 bit 185M, 720p
     8 and 10 bit: CIDs 1237 1253 1235 1251 1250), interlaced 1080 8 and 10 bit (CIDs 1242, 1241,
     field coded), 12 bit through a header patched frame, damaged frames (400 each on HQ, HQX,
     444, 1080i: no panic), the GPU picture against ffmpeg's RGB (mean abs error 0.04 / 0.01 /
     0.01 of 255), the player (order, slow polling, ring, seeks, seek and play on) and `Playback`
     with PCM (pictures follow the audio clock, seeks land on the target). The GPU upload of 8 bit
     samples is `v * 257` (exactly v / 255 when read as normalised); 10 / 12 bit go to the top bits.
   - Left: MXF (DNxHD's usual wrapper) is not read, only MOV (`AVdn` / `AVdh`); the RGB colour
     format (CLF = 1 with ACF = 0 macroblocks, what ffmpeg writes for `gbrp10`), 4:2:0, alpha
     (the lossless RLE alpha of CID 1270 and the DCT alpha planes) and CID 1260 (macroblock
     adaptive field / frame) are refused (`Error::Unsupported`); 12 bit was checked only with a
     10 bit stream whose header was patched (ffmpeg's encoder writes 10 bit for 12 bit input), not
     with a real 12 bit file; VBR CIDs (1270 to 1274) have no padding and rows are found by the
     index table, which works, but no VBR clip was available; a clip that changes size inside
     the stream is refused; the 3/4 reconstruction rule and the CID 1271 weights deserve a check
     against a file made by Avid or Resolve (none here).
7. FilmCraft removed in one step: dependency, `rf-player`'s FilmCraft code, texts and notes.
   Not before phases 4 and 6 cover what it plays today (decided 2026-11-09: no format may
   disappear in between).

Fallback: GPUs without a decoder for the stream (old cards, AV1 before 2020) keep the
Media Foundation route.

DirectLook route, gaps closed (2026-11-09, branch worktree-agent-a657e1e417248d476):
- Double click: the popup reports `WindowEvent::DoubleClick`; `poll_rd` in `video.rs` sets a flag
  and the next `render` calls `window.toggle_fullscreen()` (it has the window), as the root's
  `on_mouse_down` does for the other routes. The popup follows through the rect that `render`
  recomputes from the window bounds.
- Controls auto-hide: the popup reports `WindowEvent::MouseMove` (consecutive moves are one
  queue entry; `TrackMouseEvent` asks for `WM_MOUSELEAVE`, reported as `Leave`). The viewer runs
  `show_controls` on a move or a wheel turn and re-arms the hide timer on leave. The popup route
  no longer forces the bar on (`controls = self.controls || self.mf`). The 48 px of the bar stay
  reserved under the picture when hidden (no reflow).
- Audio device change: `audio_out` keeps its stream in a replaceable `Stream`; an
  `IMMNotificationClient` (default render / console changes) and `AUDCLNT_E_DEVICE_INVALIDATED`,
  `..._RESOURCES_INVALIDATED`, `..._SERVICE_NOT_RUNNING` ask the render thread to open the new
  default and `swap_device` the output over. Clock continues; what the old device had been given
  but not played goes back to the front of the queue (a quarter second of history is kept), so
  nothing is skipped; queue converted if rate / channels differ; mute applied to the new
  session; a running stream is restarted; without a device the clock stands still and a retry
  runs each second. `AudioOutput::force_reopen` is the test hook. Measured (muted, 3 reopens
  while playing): clock steps 9..10 ms per reopen (the 12..13 ms the reopen takes), position
  follows the clock within 3 ms, queue consumption stays on the clock, device periods keep
  coming; a pause across a reopen stays a pause; flush and a new position work afterwards.
  Not testable here: a real second device (rate change path is written, not exercised),
  an actual invalidation (the hook runs the same path).
- Pixel aspect ratio: `Track::par` from MP4 `pasp` and Matroska `DisplayWidth` /
  `DisplayHeight` (relative to the pixel size), H.264 SPS VUI `aspect_ratio_idc` / extended SAR
  (`Sps::sar`), HEVC VUI (already parsed). `StreamInfo::display` = coded width x par, coded
  height (container wins over the VUI, as in ffmpeg); AV1, VP9 and ProRes use the container's only.
  `window::shown_size` feeds the letterbox and `WindowState::video_size`; the viewer takes
  its aspect from `video_size`, so the popup rect has the display shape (the `probe`'s size is only
  the first guess). Tests (`tests/par.rs`): `setsar=4/3` H.264 and HEVC MP4 with `pasp` and with
  the `pasp` box renamed (VUI only), 12:11 from the table, MKV 4:3 and 9/8, WebM VP9, and the
  hidden window's letterbox rows (853x360 in 800x600: rows 131..469).
- Viewer moves: no fix was needed. Checked on a DirectLook test instance
  (`SMOOTHYFILES_RUSTDECODER=1`, muted, own config dir, a 640x360 `setsar=4/3` clip) driven only
  with `SetWindowPos` / `PostMessage` on its own windows: three moves of the viewer left the popup
  at the same offset (8, 43) each time, a resize to 900x700 re-fitted it (883x373, the 853:360
  shape), and a posted `WM_LBUTTONDBLCLK` on the popup took the viewer to the full 2560x1440
  monitor with the popup at 2560x1080 centred (rows 156..1236), a second one restored both. The
  popup opened at 960x405: the pixel aspect ratio reaches the viewer. Controls: the bar (screen
  capture of its strip, mean luma) was gone 4 s after opening, a posted `WM_MOUSEMOVE` on the popup
  brought it back (26.5) and it was gone again 2.5 s later (0).

## Containers: TS, PS, AVI (2026-11-09)

All three give the same `Movie` / `Track` / flat sample table. A sample is no longer always one
byte range: `Track::storage` says how it is stored, and `Track::read_sample` undoes it, so the
decoders, `AudioDecoder`, `Player` and `Playback` are unchanged. `Storage::Plain` is MP4 and
Matroska (and the AVI audio / MJPEG chunks); `Storage::Ts(Layout)` is a transport or program
stream sample; `Storage::AnnexB` is an AVI chunk of Annex B H.264. `Sample::size` is always the
number of bytes `read_sample` returns (for converted samples that is not the size in the file;
`Layout::raw_size` / `AnnexB::raw` give the stored size).

**MPEG-TS / M2TS (`ts.rs`).** Packet size 188, or 192 for AVCHD `.MTS` / Blu-ray `.M2TS` (the 4-byte
arrival time is ignored); detection = sync byte `0x47` at the starts of the first packets. The
index is one sequential pass over the file in 3 MB blocks looking at packet headers, PAT / PMT
(first program with audio or video) and PES headers; picture and frame bytes are scanned only
for what the table needs. A sample is stored as the file offset of the packet it starts in, the
elementary stream bytes to skip and to take (`Layout`, 8 bytes per sample); `Layout::read`
walks the packets of the PID from there, cuts PES headers and converts. Nothing is kept per packet.
- H.264 / HEVC (0x1B, 0x24): one PES = one access unit. Annex B becomes 4-byte length prefixed
  NAL units (access unit delimiters and filler data dropped); `avcC` / `hvcC` are made from the
  first in-band SPS / PPS (/ VPS), and give the picture size. Sync = IDR (also a recovery point
  SEI) or IRAP. PTS and DTS from the PES (DTS = PTS when only one is given); 33-bit counters are
  unwrapped per stream.
- Audio: AAC ADTS (0x0F; header cut, AudioSpecificConfig made from the first header),
  AC-3 (0x81, or 0x06 with an AC-3 descriptor / registration), E-AC-3 (0x84, 0x87, 0xA1, or 0x06),
  MPEG audio (0x03 / 0x04: layer III is `Codec::Mp3`, layers I / II are listed with
  `Codec::Other`), Blu-ray LPCM (0x80: the 4-byte header cut, 16 / 20 / 24 bit as big-endian
  16 / 24; one sample per PES). Frames are cut by their sync words and sizes across PES
  boundaries; a frame's time is the PES PTS that covers it, then running by frame length (the
  PTS only re-anchors when it is more than half a frame off). Audio track timescale = sample rate.
- MPEG-2 video (0x01, 0x02): one sample per PES (sync = I picture; the PES is taken to be one
  picture, which is what broadcast and ffmpeg do; a sample with several pictures is split by the
  decoder, a picture over several PES is not handled). Decoded by `Mpeg2Decoder` (below).
- Listed without samples: AAC in LATM (0x11, not supported), DTS, TrueHD, VC-1, subtitles, teletext.
- Times are shifted so the earliest presentation time of the file is 0 (MP4's convention: DTS of
  a B-frame stream starts negative).

**MPEG-PS (`ps.rs`).** `.mpg`, `.vob`, MPEG-1 system streams: pack headers and PES packets (MPEG-2
and MPEG-1 style headers). MPEG audio and AC-3 in private stream 1 (substream 0x80..0x87, the
4-byte private header cut) are read as frames like in TS; samples are found by walking PES packets
(`Layout` with packet size 0). MPEG-1 / 2 video is cut into pictures by `mpeg2::Splitter` while the
PES packets are read: one sample per access unit (sequence / GOP headers belong to the picture
that follows), its start found as (PES packet, bytes into its payload) like an audio frame that
starts inside a packet; sync = I picture; times from the PES where it has them (the first picture
that starts in the packet), else from the temporal references within the GOP (`mpeg2::sample_times`,
dts in decode order, never after the pts). A raw `.m2v` (starts `00 00 01 B3`) is read the same way
with plain storage (`mpeg2::read_elementary`). H.264 / HEVC / LPCM / DTS in program streams: not
handled.

**AVI (`avi.rs`).** `hdrl` (`avih`, `strl` with `strh` / `strf` / `indx`), chunks from the OpenDML
super index and `ix##` chunk indexes (files with `RIFF AVIX` parts, > 1 GB) where every stream has
one, else `idx1` (offsets relative to `movi` or absolute, probed), else a walk over the `movi` lists
(`LIST rec ` entered; a cut file keeps its whole chunks). Entries outside the file are dropped.
- Video time = `n * dwScale` in `dwRate`; zero-sized chunks (dropped frames) take a slot, no sample.
- H.264 / HEVC (`H264`, `X264`, `avc1`, `davc`, `HEVC`, ...): Annex B chunks, `avcC` / `hvcC` made
  from the in-band parameter sets, converted on read. Every picture is read once at open to get
  its converted size (and its sync flag when the file has no index). AVI has no presentation
  times: for H.264 the display order comes from the POC (type 0) of each picture's first slice
  header; `pts` is the display slot, `dts` = decode slot minus the largest reordering delay. HEVC
  gets `pts = dts` (B-frame HEVC in AVI would show out of order), POC types 1 / 2 as well. An `avc1`
  stream that is length prefixed with an `avcC` in `strf` is read as is (not covered by a test:
  ffmpeg always writes Annex B).
- Motion JPEG and other video: listed (`Codec::Other`), every frame a sync sample, not decoded.
- Audio: PCM (8 / 16 / 24 / 32 bit, float), MP3 (0x55), AAC (0xFF with `AudioSpecificConfig`),
  AC-3 (0x2000), `WAVE_FORMAT_EXTENSIBLE` unwrapped. PCM chunks count `size / blockAlign`
  samples, compressed chunks one `dwScale`; the timescale is `dwRate` (ffmpeg's MP3 AVI uses a
  125 Hz clock: 24 ms ticks). Everything else (ADPCM, WMA, ...) is listed without samples.

**Tests** (`tests/ts.rs`, `tests/avi.rs`, `tests/container_robust.rs`, clips made by ffmpeg into the
temp folder; times against `ffprobe -show_packets -show_data_hash CRC32`):

| clip | video | audio | compared with ffprobe |
| --- | --- | --- | --- |
| `.ts` | H.264 B-frames, 90 pkts, 3 sync | AAC 142 | count, pts, dts, sync; video size + CRC32 of the stream bytes; AAC size (+ 7 byte header) |
| `.ts` | HEVC, 90 pkts | AAC 142 | same |
| `.m2ts` (`-mpegts_m2ts_mode 1`) | H.264 | AC-3 94 | same, audio size + CRC32 |
| `.ts` | H.264 | E-AC-3 94 | same |
| `.m2ts` | H.264 | `pcm_bluray` 16 bit 600, 24 bit 400 | same, LPCM size (+ 4 byte header) |
| `.ts` | H.264 | MP3 126 | same |
| `.ts` | MPEG-2 (8 I pictures of 90) | MP2 125 | same (listing only) |
| `.vob`, `.mpg` (MPEG-2), `.mpg` (MPEG-1) | 90 pictures each: pts (where ffprobe has one), size, CRC32, key | AC-3 94, MP2 125, MP2 125 | audio frames: pts, size, CRC32 |
| `.avi` | H.264 | PCM 141 | count, sync, size + CRC32 of every chunk, dts (= pts) |
| `.avi` | H.264 with B-frames | MP3 126 | same minus pts / dts (ffprobe shows none): pts a dense display run, dts <= pts |
| `.avi` | H.264 | AAC 142, AC-3 94 | same |
| `.avi` | MJPEG (listed) | PCM | count, sync, size + CRC32 |
| hand-built | MJPEG / PCM | | OpenDML super index over two RIFF parts + `ix##`; `idx1` relative and absolute; walk without index with `LIST rec `; a cut file |
| real 1.15 GB OpenDML (not in the suite: `big_opendml`, `--ignored`) | raw video 3300 | PCM 5157 | all packets, last frame at byte 1150958894 (past 1 GB, found through the `ix##` chunks) |

The NAL lengths of every converted H.264 / HEVC sample add up to the sample. Decoded on the GPU
and compared with ffmpeg's software decoder picture by picture (NV12, all 90 frames, identical):
H.264 + AAC `.ts`, HEVC + AAC `.ts`, H.264 + AC-3 `.m2ts`, H.264 `.avi` and H.264 B-frame `.avi`
(display order from POC). `Playback` on H.264 + AAC `.ts` with the fake sink: pictures in order
and on the audio clock, 75 pictures in 2.5 s, none dropped, audio one unbroken run; seeks land
audio at the target to 1/48000 s and video on the picture at or before it (same checks as MP4).
`container_robust`: 161 cut / bit-flipped / zeroed variants of each of `.ts`, `.m2ts`, `.avi`,
`.vob` read without a panic.

**Speed.** `--example index_time <file>`: a 1.12 GB 1080p 24 Mb/s TS (8 copies of a 45 s clip,
10800 pictures, 16888 AAC frames) is indexed in 0.35 s (3.2 GB/s, file in the OS cache; a cold
disk is limited by the disk, the pass is sequential). A sample read is 0.01 to 0.5 ms (a 515 KB
picture 0.4 ms). The 1.15 GB OpenDML AVI is indexed in a few milliseconds (index chunks only).
No AVCHD file was available in the camera folder, so no camera stream was tried.

**Left.**
- A real camera stream: AVCHD with interlaced H.264 (decoding is tested on x264 / NVENC streams
  in `.m2ts`, not on a camera file), PTS jumps, several programs, 0x06 streams with unusual descriptors.
- TS: resync after lost sync inside a file (the rest of the file is ignored at the first bad
  sync byte), PES with several access units or an access unit over several PES (split by AUD),
  AAC LATM, HE-AAC signalling, DTS / TrueHD / VC-1 / PGS, an `.m2ts` arrival-time clock, several
  programs (the first with audio or video is used), PTS discontinuities (segments that start
  over, as concatenated files do).
- PS: PSM-signalled H.264, DVD LPCM / DTS. (The MPEG-1 / 2 picture splitter and raw `.m2v` are
  done, see "MPEG-1 / MPEG-2 video".)
- AVI: HEVC / POC types 1 and 2 reordering, `avc1` with an `avcC` (untested), packed B-frame
  bitstreams (DivX / MPEG-4 part 2 are listed only), `indx` sub-index types other than chunk indexes
  (field indexes), a cheaper H.264 open (the pictures are read once for their size; 1 GB of
  H.264 takes about as long as a TS index).

Still to do before the route can be the default: formats the Media Foundation / FilmCraft
routes play that the container readers do not take (WMV / ASF, FLV; the other codecs listed under
"Containers: TS, PS, AVI" are read but not decoded); several audio tracks and track choice, subtitles; playback keys
### MPEG-1 / MPEG-2 video on the GPU (done 2026-11-09)

Files: `mpeg2.rs` (headers, splitter, raw ES reader), `dxva_mpeg2.rs` (DXVA 1.0 structures),
`mpeg2dec.rs` (`Mpeg2Decoder`, `VideoDecoder::Mpeg2`), `tests/gpu_mpeg2.rs`,
`tests/playback_mpeg2.rs`, `examples/mpeg2_profiles.rs` (lists the decoder profiles).

- Headers parsed: sequence header + extension (size with extension bits, profile / level,
  progressive_sequence, chroma format, low delay), quantiser matrices (default, loaded in the
  sequence header, quantiser matrix extension; kept until the next sequence header; stored in scan
  order as DXVA wants them), GOP (closed_gop), picture header + coding extension (f_codes,
  intra_dc_precision, structure, top_field_first, frame_pred_frame_dct, concealment motion vectors,
  q_scale_type, intra_vlc_format, alternate_scan, repeat_first_field, chroma_420_type,
  progressive_frame) and, per slice, the row, the quantiser scale code and the bit offset of the
  first macroblock from the start code (extra slice info skipped). The macroblock layer is the video
  engine's: we never decode a coefficient.
- GPU: `D3D11_DECODER_PROFILE_MPEG2_VLD` (`MPEG2and1_VLD` as a second choice), NV12, ConfigBitstreamRaw
  1. `DXVA_PictureParameters` + `DXVA_QmatrixData` (all four matrices every picture) + one
  `DXVA_SliceInfo` per slice (slices with their start codes packed into the bitstream buffer;
  `wNumberMBsInSlice` from the distance to the next slice's row, so a slice is taken to start at
  the left edge: several slices in one row get the wrong count). Layouts follow ffmpeg's
  `dxva2_mpeg2.c`; checked by size in a unit test.
- Order: B pictures go out when decoded; an I / P picture waits for the next I / P (backward
  reference). Two reference surfaces, 8 in the pool. Seeking to a key sample drops the B pictures
  that need the GOP before (unless the GOP is closed). A sequence header with a new size starts a
  new decoder; what was queued is dropped (a mid-stream size change loses a picture).
- Pictures are cropped to the sequence size (1080 lines out of 1088, checked).
- Surface alignment 32 (rows of macroblocks are rounded to even for interlaced sequences).

Measured (`tests/gpu_mpeg2.rs`; RTX 5070 Ti, ffmpeg 8.1.1 software decoder as reference through
`-f rawvideo -pix_fmt nv12`, every sample of every frame; the MPEG-2 IDCT on the GPU is not
ffmpeg's, so the pictures are not bit-exact; error grows along a GOP as prediction carries it):

| Clip | Frames | max abs diff | samples that differ | histogram of abs diff 0 / 1 / 2 / 3 / 4+ |
|---|---|---|---|---|
| 640x352 PS, g 12, no B | 75 | 6 | 4.80 % | 24127970 / 1172330 / 40469 / 2858 / 373 |
| 640x352 PS, g 12, 2 B | 75 | 4 | 3.51 % | 24453843 / 879210 / 10809 / 134 / 4 |
| 720x576 interlaced (ilme+ildct, top first) `.vob`, 2 B | 50 | 4 | 2.41 % | 30353741 / 742765 / 7401 / 88 / 5 |
| 640x352 TS, custom intra / inter matrices, 2 B | 50 | 4 | 4.25 % | 16177920 / 712983 / 5047 / 49 / 1 |
| 640x352 PS, non-linear q scale + intra VLC + alternate scan | 50 | 3 | 2.57 % | 16461230 / 431379 / 3383 / 8 / 0 |
| 1280x720 TS, 2 B | 50 | 3 | 1.60 % | 68014938 / 1096592 / 8443 / 27 / 0 |
| 1920x1080 interlaced TS (1088 coded), 2 B | 25 | 3 | 0.92 % | 77045181 / 709859 / 4918 / 42 / 0 |
| 640x352 raw `.m2v`, 2 B | 50 | 4 | 2.67 % | 16444602 / 445349 / 5992 / 55 / 2 |
| 720x576 MXF D-10-like intra only (30 Mb/s) | 50 | 1 | 3.18 % | 30115316 / 988684 / 0 / 0 / 0 |
| 720x576 MXF long GOP (g 6, 2 B) | 50 | 3 | 3.01 % | 30167713 / 928593 / 7681 / 13 / 0 |

The test holds the largest difference to 12 and 98 % of the samples to within 1. Presentation
times increase in display order in every clip. Also checked: PS video samples against ffprobe
(`tests/ts.rs`: 90 pictures each in `.vob` / `.mpg` MPEG-2 and MPEG-1: size, CRC32, key, pts
within 1 tick); seeking to a key sample (39 pictures after it equal the full decode's, the first
at the key's time); cut and bit-flipped program streams (no panic); the MP2 sound.

- **MP2 audio** (MPEG-1 layer II in PS / TS / MXF `mp2 ` tracks): Windows has "Microsoft MPEG Audio
  Decoder MFT" for `MFAudioFormat_MPEG`; `MftCodec::Mp2` in `audio.rs` (an additive change: the
  MPEG1WAVEFORMAT tail as user data, the track found by its `mp2 ` fourcc). Against `ffmpeg -f f32le`
  (PS, 3 s stereo 48 kHz): 144000 frames each, shift 0, max diff 3e-5, rms 1.2e-5.
  `tests/playback_mpeg2.rs`: MPEG-2 PS + MP2 through `Playback` with a fake sink and fake time
  (38 pictures in 1.5 s at 25 fps, none dropped, audio one unbroken run, three seeks land on the
  picture at or just before the target).
- **MPEG-1 video is refused** (`Unsupported`): on this NVIDIA driver `MPEG2and1_VLD` (and
  `MPEG2_VLD`) decode the first macroblocks of an MPEG-1 picture and then lose the bitstream (the
  picture is mostly wrong, even an I picture, mean abs error 92 of 255). Nothing in the DXVA
  structures says MPEG-1 (escape codes and the dequantiser differ); tried: the other profile,
  `bMVprecisionAndChromaRelation` 0..3, PCE bits zero. ffmpeg itself has no DXVA MPEG-1 path.
  Not shown wrong; the container reader still lists MPEG-1 tracks with samples.
- **4:2:2** (D-10 50, XDCAM HD422): D3D11 has no 4:2:2 MPEG-2 decoder profile; refused with a message
  (tested with a 4:2:2 PS). XDCAM HD 35 (4:2:0) in MXF is the same path as the 4:2:0 clips above.
- **Field pictures** (two fields into one surface, `bSecondField`, the first field of the same frame
  as a reference of the second): **not done**, refused with a message. ffmpeg has no encoder that
  writes them (its mpeg2video makes frame pictures with interlaced DCT / `ilme`) and no such file was
  on this machine, so there was nothing to check an implementation against; the DXVA reference
  semantics of the second field are the part that would be guessed. Frame pictures of interlaced
  material (what DVDs and most encoders write) work.
- Not done / limits: slices that do not start at the left edge (several per row) get a wrong
  `wNumberMBsInSlice`; a picture spread over several PES in a transport stream (TS keeps one sample
  per PES; PS and raw ES are split properly); `repeat_first_field` / 3:2 pulldown is not used for
  timing (one picture, one time, like ffmpeg's `-fps_mode passthrough`); sequence display extension
  (colours: BT.709 for 720 lines and up, else BT.601, guessed); the viewer route takes `.mpg`, `.mpeg`,
  `.vob`, `.m2v` (and TS / MXF) for MPEG-2 4:2:0 now (`rd_video.rs`, not compiled here).

### MXF (OP1a, OP-Atom) done 2026-11-09

`mxf::parse` / `mxf::read` / `container::read` (an MXF is recognised by the header partition
key at 0, or after a run-in of up to 64 KiB). What it does:

- One linear scan of KLV triplets (BER lengths). Partition packs (header / body / footer, open or
  closed, complete or not), the primer, the header metadata local sets, index table segments and
  the essence elements are told apart by key; only key and length of an essence element are read.
  Measured: a 212 MB OP1a file (20000 H.264 frames + 20000 PCM elements, 800 s, 80 index
  segments) is indexed in 64 ms with the file in the cache (`mxf_dump`).
- Metadata: Preface, ContentStorage, EssenceContainerData, the file SourcePackage (the one with a
  picture or sound descriptor, the one the container data names first), its timeline Tracks
  (TrackID, TrackNumber, EditRate, Origin), Sequence, descriptors (CDCI / RGBA / MPEG video:
  stored and display size, display aspect, frame layout, component depth, subsampling, picture
  essence coding label; WaveAudio / AES3 / generic sound: rate, channels, bits, block align,
  coding label; MultipleDescriptor). Every header copy (each primer starts a generation) is tried
  from the last to the first: a file that grew gets the footer's copy.
- Codecs from the picture essence coding label, then the essence container label: AVC (`04 01 02
  02 01 31|32`, container `0D 01 03 01 02 10`), HEVC (`..01 33..3F`, unverified: no sample), VC-3
  (`..02 71`, the byte after is the compression ID less 1235: 1270 and up gets `AVdh`, else
  `AVdn`), ProRes (RDD 44 `..03 06 <profile>` to `apco` .. `ap4x`), MPEG-2 and JPEG 2000 (listed
  with their sample tables, `Codec::Other`, fourcc `mpg2` / `mjp2`, no decoder), uncompressed.
- Frame wrapping: one sample per essence element, offset at the element's value, so KLV never
  reaches a decoder. Clip wrapping (ffmpeg's and Avid's OP-Atom video): the single element is cut
  by the index table, a constant `EditUnitByteCount` or the entries' stream offsets. PCM
  (BWF) is cut into blocks of one edit unit when the edit rate is a frame rate, else (Avid's
  audio files have the sample rate as edit rate) 1024 samples; sample times are in samples,
  timescale = the sample rate.
- Index tables: key frames and temporal offsets of Long GOP streams. Read the way ffmpeg's
  demuxer reads them (it plays Sony, Panasonic and its own files): the entry at position x says
  the picture stored at x + offset is shown at x; key = points at itself and no prediction flags
  (`flags & 0x33 == 0`); `dts` = stored position less the largest offset, so the first picture
  is shown at 0 and decoded before it (like an MP4 `ctts` with an edit list). Without an index
  entry a frame's key flag comes from the stream (H.264 IDR, HEVC IRAP, MPEG-2 sequence / GOP
  header), intra codecs are all key.
- H.264 / HEVC: Annex B in the file; the track gets an `avcC` / `hvcC` made from the first frames
  that hold parameter sets, `Track::read_sample` makes each frame length-prefixed (it tells the
  forms apart by a leading `00 00 00 01`, which a length-prefixed sample cannot start with), so
  `H264Decoder` reads MXF through `container::read` unchanged.
- AES3 sound elements (D-10, SMPTE 386M: 4-byte header with the sample count, 32-bit subframes):
  a PCM track with fourcc `aes3`, `read_sample` returns plain interleaved little-endian PCM.
  Some writers (ffmpeg's OP1a) put the AES3 label on plain PCM: the header has to fit the
  element's size in the first three elements, else it is read as plain PCM.
- Cut and growing files: no footer or index is needed (the sample table comes from the
  elements), a last element cut in the middle is dropped, `Mxf::incomplete` says the header was
  open or the scan hit a cut. Run-in is skipped (`Mxf::run_in`).
- OP-Atom: one essence per file. `Mxf::material_package` (shared by the files of a clip) and
  `Mxf::related_packages` (the file packages its source clips name) are there for linking the
  separate audio `.mxf`s to the video file; nothing links them yet.

Measured (`tests/mxf.rs`, files from ffmpeg's `mxf`, `mxf_opatom`, `mxf_d10`; the table of the
test prints on `--nocapture`):

| clip | pattern / wrapping | tracks | against `ffprobe -show_packets` |
| --- | --- | --- | --- |
| DNxHD 1080p 120M + PCM 24 bit stereo | OP1a, frame | VC-3, PCM | 10 + 10 packets: count, size, CRC of bytes, key, pts, dts equal |
| DNxHD 720p 90M | OP-Atom, clip, CBR index | VC-3 | 10 equal |
| DNxHR HQ 640x360 + PCM 16 bit 6 ch | OP1a, frame | VC-3 (`AVdh`), PCM | 25 + 25 equal |
| DNxHR HQ | OP-Atom, clip | VC-3 | 25 equal |
| ProRes 422 HQ 720p + PCM | OP1a, frame | ProRes (`apch`), PCM | 10 + 10 equal |
| H.264 Long GOP (g 12, 2 B-frames) + PCM | OP1a, frame, footer index | AVC, PCM | 50 + 50 equal (5 IDR) |
| H.264 intra (`-g 1`) | OP1a | AVC | 20 equal |
| MPEG-2 Long GOP (g 6, 2 B-frames) + PCM | OP1a, index with temporal offsets | MPEG-2 (decoded since: see its section) | 25 + 25 equal, dts -1 frame like ffprobe |
| MPEG-2 D-10 50 Mb/s + 4 ch AES3 24 bit | OP1a, CBR index | MPEG-2, AES3 PCM | 10 + 10 equal, audio bytes after conversion equal |

Decoding what came out of MXF: DNxHD / DNxHR 1080p, 720p and 640x360 through `DnxDecoder`
(worst sample error 1 against ffmpeg, 2 for the 720p 90 Mb/s clip; the same packets in a MOV give
identical pictures, so the 2 is the DNx decoder's rounding, not the container's), ProRes HQ
(error 1), H.264 Long GOP (50 frames) and intra (20 frames) on the GPU bit for bit in display
order (the reference is the raw stream: ffmpeg's own MXF demuxer numbers the pictures of the
files its muxer wrote in decode order, the muxer's index has no temporal offsets for AVC), PCM
through `AudioDecoder` (24 bit, 16 bit, 6 channels, D-10 AES3: identical to ffmpeg's float
output), one OP1a DNxHR + PCM file through `Playback` with a fake clock (23 pictures in 0.9 s at
60 Hz, none dropped, audio one run from 0 and on the clock). Structure: a run-in of 1000 bytes, a
file cut in the middle of frame 6 with no footer (6 frames kept, keys from the stream), and a
hand-built open-header file with clip-wrapped PCM at edit rate 25 and 48000 (blocks of 1920 and
1024).

Not done / left:
- Files from ffmpeg's muxer for AVC carry no temporal offsets: B-frame pictures come out of the
  decoder in display order but with `pts` of the decode order (the sample table has no other
  information). Real recorders write the offsets; for the others the picture order count of the
  slice headers would give the times.
- Several mono audio tracks (cameras write 2 to 8): each is its own `Track`; `Movie::audio_tracks()`
  offers them as one multichannel entry (see "Several audio tracks" below).
- OP-Atom clips are not linked (video file and the audio `.mxf`s); essence in more than one
  BodySID (OP2x, OP3x) is read as one stream; encrypted essence, JPEG 2000 decoders (MPEG-2: see its section),
  HEVC and ProRes-by-profile on real camera files (no samples here; the HEVC mapping label is
  from the standard, not tested), AVC with 3-byte NAL lengths from other writers (always Annex
  B here), the RIP and the footer partition's `PreviousPartition` chain are not used (the scan
  needs neither).
- The viewer route (`rd_video.rs`) only lists `mp4 mov m4v mkv webm`: `mxf` has to be added there.
- No real camera file was available (nothing under `E:\gelen works 2026`); `RD_MXF=<file>` runs
  `real_camera_mxf_if_there_is_one` on one.

Still to do before the route can be the default: formats the Media Foundation / FilmCraft
routes play that the container readers do not take (AVI, MPEG-TS / PS, WMV / ASF, FLV, ...
DNxHR in progress elsewhere); several audio tracks and track choice, subtitles; playback keys
(arrows, space, F) go to the viewer window, not checked with the popup in front; cursor hiding
in fullscreen; HDR metadata in the tone mapper; a mid-stream size change; a real second audio
device for the rate-change path; a hot-plug test with a physical device.

## Several audio tracks (2026-11-09)

**Listing.** `Movie::audio_tracks()` returns one `AudioTrack` per choice: `members` (indices into
`Movie::tracks`), `codec` name, `channels`, `sample_rate`, `language`, `name`, `default`. Tracks
carry `Track::meta` (`TrackMeta { language, name }`): Matroska `LanguageBCP47` ("IETF") over
`Language`, and `Name`; MP4 / MOV `mdhd` language (ISO packed letters, or QuickTime's old numeric
codes mapped to ISO 639-2) and the title from `udta/name`, else the `hdlr` name unless it is a
muxer's stock text ("SoundHandler"). `default`: Matroska `FlagDefault` (true when absent); MP4's
`tkhd` enabled flag (ffmpeg clears it on the audio tracks that are not the default disposition).
`Movie::default_audio_track(&list)` is the first marked default, else the first.

**Camera mono tracks.** Two or more mono PCM tracks with the same sample entry, rate, sample count
and length (within 1 ms) and no two different languages are one entry: `channels` = their number,
track 1 = channel 1 (left), 2 = right, and so on. The language rule keeps a film's dubbed mono PCM
tracks (MKV, MOV) from being merged. `AudioDecoder::open_choice(path, &movie, &entry)` opens a
group as one decoder with a decoder per member underneath; blocks are cut so that frame `n` of
every channel is the same instant (a member that is shorter is silent past its end), after a seek
too. The group equals the tracks interleaved bit for bit (tests against ffmpeg's decode of each
track: MOV and MXF with 4 mono PCM tracks, MXF with 8). `AudioDecoder::open` takes the default
entry, so a camera file now plays all of its tracks.

**Playback.** `Playback::open*` opens the default entry, or the first that opens when the default
is not playable (a codec without a decoder). `audio_tracks()`, `audio_track()`, and
`set_audio_track(i)`: the new decoder is opened on the caller's thread while the old track keeps
playing, then the feed thread swaps it, seeks it to the clock time, and flushes the sink once
(playing or paused; the video and the clock are not touched; about 3 ms here). A group of more
than two channels reaches the sink as its first two (left, right): the sink's 5.1 / 7.1 matrices
assume surround layouts, which camera tracks are not. The other channels stay in the decoder
(`AudioDecoder::next_block`).

Not done: a UI to choose (DirectLook); choosing which mono tracks form the pair, or mixing all of
them; non-PCM mono tracks (AAC and the like) are never grouped; MXF track names / languages (the
MXF reader fills no `meta`); AVI / TS / PS have no language either.
