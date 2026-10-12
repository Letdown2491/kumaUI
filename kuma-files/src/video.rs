//! In-process video decode and playback: kumaOS ships the full libav
//! as ffmpeg-libs and no CLI (the kumaOS ffmpeg contract), so
//! posters, preview loops, durations and the Quick Look player come
//! from linking libavcodec, libavformat and libswscale. Sound comes
//! from libswresample plus the sound server's pulse-simple entry
//! point, dlopen'd (never linked), and any gap in that chain degrades
//! to muted playback with the wall clock. Videos are hostile input:
//! every step is fallible and degrades, so callers fall back to the
//! type icon or the card. No function here may run libav on the UI
//! thread; callers wrap the decode in catch_unwind like every other
//! thumbnail decode, and the player's loop runs on its own thread
//! (`Player::try_recv` only drains the ring, `Player::clock_micros`
//! only reads the clock's shared state).
//!
//! The portability rule: libav is a capability, not a requirement.
//! The decode path lives behind the `video` feature (default on);
//! without it this module is the stub below and video is a degraded
//! feature: the type icon and the plain card stand in.
//!
//! A base image update that moves libav (even within one soname)
//! leaves the bindgen'd struct offsets stale in target/: decode
//! then corrupts instead of failing (fields land at old offsets).
//! `cargo clean -p ffmpeg-sys-next` after any libav bump.

/// Why a video has no poster frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PosterFail {
    /// libav parsed the container but ships no decoder for the
    /// codec (Fedora's stripped libavcodec-free has no H.264 or
    /// H.265, for example): a named state, never a generic failure
    /// and never a panic
    CodecMissing,
    /// unreadable, truncated, or no video stream
    Empty,
}

/// Seconds into a wall-clock label ("3.42" reads as "0:03", "3621.4"
/// as "1:00:21"). NaN, negatives, and other junk read as None.
pub(crate) fn duration_label(seconds: f64) -> Option<String> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let total = seconds.round() as u64;
    let (h, rem) = (total / 3600, total % 3600);
    let (m, s) = (rem / 60, rem % 60);
    if h > 0 {
        Some(format!("{h}:{m:02}:{s:02}"))
    } else {
        Some(format!("{m}:{s:02}"))
    }
}

/// A stream with degenerate timestamps (static, or none at all)
/// must not spin the img element's frame clock at unbounded rate:
/// every displayed frame gets at least this delay.
#[cfg(feature = "video")]
pub(crate) const PREVIEW_MIN_DELAY: std::time::Duration = std::time::Duration::from_millis(16);

/// The presentation delay a frame implies, from PTS deltas in
/// microseconds: the delta to the next frame, floored at the pace
/// floor. An unknown timestamp or a backwards one carries the
/// previous delay (the floor at the head).
#[cfg(feature = "video")]
pub(crate) fn delay_from_pts(
    prev_pts: Option<i64>,
    pts: Option<i64>,
    prev_delay: std::time::Duration,
) -> std::time::Duration {
    match (prev_pts, pts) {
        (Some(prev), Some(pts)) if pts > prev => {
            std::time::Duration::from_micros((pts - prev) as u64).max(PREVIEW_MIN_DELAY)
        }
        _ => prev_delay,
    }
}

#[cfg(feature = "video")]
mod imp {
    use super::{PREVIEW_MIN_DELAY, PosterFail, delay_from_pts};
    use std::ffi::CString;
    use std::path::Path;
    use std::sync::atomic::Ordering::{Acquire, Release};

    use ffmpeg_sys_next as sys;

    /// libav logs misdetections straight to stderr; pin it to error
    /// lines so the journal stays clean.
    fn quiet_logs() {
        unsafe { sys::av_log_set_level(sys::AV_LOG_ERROR as i32) };
    }

    /// A path as a C string, None on interior NULs.
    fn cpath(path: &Path) -> Option<CString> {
        CString::new(path.as_os_str().as_encoded_bytes()).ok()
    }

    /// RAII for the demuxer: close frees the context and its streams.
    ///
    /// `Send` so the player can move an opened context into its
    /// decode thread: the move transfers ownership, the open ran to
    /// completion on the spawning thread, and libav's own rule is
    /// only that no two threads touch one context at once.
    struct FormatInput(*mut sys::AVFormatContext);

    unsafe impl Send for FormatInput {}

    impl Drop for FormatInput {
        fn drop(&mut self) {
            unsafe { sys::avformat_close_input(&mut self.0) };
        }
    }

    /// RAII for the decoder context: free closes and frees. `Send`
    /// for the player thread, same reasoning as `FormatInput`.
    struct CodecCtx(*mut sys::AVCodecContext);

    unsafe impl Send for CodecCtx {}

    impl Drop for CodecCtx {
        fn drop(&mut self) {
            unsafe { sys::avcodec_free_context(&mut self.0) };
        }
    }

    /// RAII for the scaler.
    struct Scaler(*mut sys::SwsContext);

    impl Drop for Scaler {
        fn drop(&mut self) {
            unsafe { sys::sws_freeContext(self.0) };
        }
    }

    /// RAII for a packet: allocated once per decode, unref'd per step.
    struct Packet(*mut sys::AVPacket);

    impl Packet {
        fn alloc() -> Option<Self> {
            let pkt = unsafe { sys::av_packet_alloc() };
            (!pkt.is_null()).then(|| Self(pkt))
        }
    }

    impl Drop for Packet {
        fn drop(&mut self) {
            unsafe { sys::av_packet_free(&mut self.0) };
        }
    }

    /// RAII for a frame: allocated once per decode, unref'd per step.
    struct Frame(*mut sys::AVFrame);

    impl Frame {
        fn alloc() -> Option<Self> {
            let frame = unsafe { sys::av_frame_alloc() };
            (!frame.is_null()).then(|| Self(frame))
        }
    }

    impl Drop for Frame {
        fn drop(&mut self) {
            unsafe { sys::av_frame_free(&mut self.0) };
        }
    }

    /// Whether this system's libav has no decoder for the codec
    /// (avcodec_find_decoder came back NULL): the named state's
    /// trigger.
    pub(crate) fn decoder_missing(codec_id: sys::AVCodecID) -> bool {
        unsafe { sys::avcodec_find_decoder(codec_id).is_null() }
    }

    /// The best video stream's index and codec parameters, or None
    /// when the file has no video stream.
    fn best_video_stream(
        ic: *mut sys::AVFormatContext,
    ) -> Option<(usize, *mut sys::AVCodecParameters)> {
        let index = unsafe {
            sys::av_find_best_stream(
                ic,
                sys::AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                std::ptr::null_mut(),
                0,
            )
        };
        if index < 0 {
            return None;
        }
        let streams = unsafe { *std::ptr::addr_of!((*ic).streams) };
        let stream = unsafe { *streams.add(index as usize) };
        let par = unsafe { (*stream).codecpar };
        Some((index as usize, par))
    }

    /// The container header's facts: duration in seconds when the
    /// container reports one, plus the coded dimensions, straight
    /// from the codec parameters: no frame is decoded and no decoder
    /// is opened, so the facts land even where libav lacks the
    /// codec's decoder.
    pub(crate) fn probe(path: &Path) -> Option<(f64, u32, u32)> {
        quiet_logs();
        let path = cpath(path)?;
        unsafe {
            let mut ic: *mut sys::AVFormatContext = std::ptr::null_mut();
            if sys::avformat_open_input(
                &mut ic,
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
            ) < 0
            {
                return None;
            }
            let input = FormatInput(ic);
            if sys::avformat_find_stream_info(input.0, std::ptr::null_mut()) < 0 {
                return None;
            }
            let raw = (*input.0).duration;
            let duration = (raw >= 0).then(|| raw as f64 / 1_000_000.0)?;
            let (_, par) = best_video_stream(input.0)?;
            let (width, height) = ((*par).width as u32, (*par).height as u32);
            (width > 0 && height > 0).then_some((duration, width, height))
        }
    }

    /// One representative frame as a BGRA `RenderImage`, no more than
    /// `max` on the long edge: ten percent in, one second as the
    /// ceiling (first frames run black on plenty of recordings), with
    /// a first-frame retry when the seek or the decode at that point
    /// comes up empty. `Err(CodecMissing)` when this system's libav
    /// parses the container but has no decoder for the codec.
    pub(crate) fn decode_poster(path: &Path, max: u32) -> Result<gpui::RenderImage, PosterFail> {
        poster_dynamic(path, max).map(|image| crate::icons::decode_to_render(image, max, max))
    }

    /// The details pane's preview budget: at most this many frames
    /// and at most this much source time per loop, whichever caps
    /// first; a clip shorter than the budget loops whole. The caps
    /// bound both the decode work (one background pass per settled
    /// selection) and the memory (fitted frames only).
    const PREVIEW_MAX_FRAMES: usize = 96;
    const PREVIEW_MAX_SECS: f64 = 8.0;
    /// The preview's memory ceiling on top of the frame cap: the
    /// frame count alone does not bound a caller that passes a large
    /// `max`.
    const PREVIEW_MAX_BYTES: usize = 64 * 1024 * 1024;

    /// The clip's head as a BGRA multi-frame `RenderImage`: a bounded
    /// loop of fitted frames whose per-frame delays are the stream's
    /// own PTS deltas, for the img element to cycle (it honors
    /// reduce_motion and parks when the window is inactive). Falls
    /// back to the single-frame poster when the head could not be
    /// turned into a loop (one decodable frame, a decode that came
    /// up empty); `Err(CodecMissing)` and `Err(Empty)` exactly where
    /// decode_poster errs. Never runs on the UI thread.
    pub(crate) fn decode_preview(path: &Path, max: u32) -> Result<gpui::RenderImage, PosterFail> {
        let cpath = cpath(path).ok_or(PosterFail::Empty)?;
        let start = std::time::Instant::now();
        match preview(&cpath, max) {
            Ok(frames) if frames.len() >= 2 => {
                let bytes: usize = frames.iter().map(|f| f.buffer().as_raw().len()).sum();
                log::info!(
                    "video preview: {} frames, {:.1} MB, decoded in {} ms",
                    frames.len(),
                    bytes as f64 / (1024.0 * 1024.0),
                    start.elapsed().as_millis()
                );
                Ok(gpui::RenderImage::new(smallvec::SmallVec::from_vec(frames)))
            }
            // the poster stands in: the loop is not worth showing
            _ => decode_poster(path, max),
        }
    }

    /// The playback decode cap on the long edge: the contract's bar
    /// is 1080p software playback, so larger sources scale down in
    /// the scaler (never up), a portrait clip caps its long edge
    /// too, and the pane's own fit does the rest. One 1080p BGRA
    /// frame is about 8 MB.
    const PLAYER_MAX: u32 = 1080;
    /// Frames in flight from the decode thread to the pane: a
    /// handful is decode-ahead enough at 25fps (about 160ms) and
    /// doubles as the pause buffer, because the thread blocks on a
    /// full ring, which is exactly what a paused pane wants it to
    /// do.
    const PLAYER_RING: usize = 4;

    /// What the decode thread ships the pane: a frame, or the
    /// stream's end. `Eof` also covers a decode error mid-stream, so
    /// the pane parks cleanly; a thread that dies without sending
    /// (a panic) reads the same through the disconnected channel.
    pub(crate) enum PlayerMsg {
        /// A decoded frame, fitted, its media time in seconds, and
        /// the seek generation it was decoded in (frames from before
        /// the pane's latest seek are stale and dropped unheard).
        Frame {
            image: std::sync::Arc<gpui::RenderImage>,
            pts: f64,
            generation: u32,
        },
        /// The stream is over; every frame is in the pane's hands.
        Eof,
    }

    /// The audio clock, pure: the media time the sound server is
    /// playing, from the end of the last chunk written to the sink,
    /// the latency sampled at that write, and the time since. The
    /// sink plays its latency down after the write; once it
    /// underruns the heard time sits at the write's end until the
    /// next write re-anchors. Table-tested; `ClockShare` is this
    /// plus the shared reads and writes.
    pub(crate) fn heard_micros(pts_written: i64, latency: u64, since_write: u64) -> i64 {
        let behind = latency.saturating_sub(since_write) as i64;
        pts_written.saturating_sub(behind)
    }

    /// The audio clock's shared state, plus the pane's two levers on
    /// the decode thread. The thread publishes per audio chunk; the
    /// pane's present tick reads lock-free, so a clock read is
    /// always safe on the UI thread.
    struct ClockShare {
        /// the pane's pause: the decode loop checks it per packet
        /// and parks. The sink's bounded tail drains out during the
        /// pause, and the resume continues at the audio's own
        /// position, so sound and picture stay locked (the picture
        /// jumps the tail's worth, at most a buffer length).
        paused: std::sync::atomic::AtomicBool,
        /// integer gain applied in the S16 conversion, 256 = unity
        volume: std::sync::atomic::AtomicU32,
        /// whether there is anything to hear: false once the sink is
        /// absent or died, which sends the clock back to the wall
        audible: std::sync::atomic::AtomicBool,
        /// the pane's seek request: one flag and the target in
        /// container-relative micros. The loop swaps the flag off and
        /// jumps; a seek also bumps `generation`.
        seek_request: std::sync::atomic::AtomicBool,
        seek_target: std::sync::atomic::AtomicI64,
        /// bumped after every completed seek; the frames carry the
        /// value they were decoded under, and the pane drops
        /// anything at or before its last seen generation
        generation: std::sync::atomic::AtomicU32,
        /// micros of media time at the END of the last chunk written
        /// to the sink; i64::MIN until the first write lands
        pts_written: std::sync::atomic::AtomicI64,
        /// the sink's latency, in micros, sampled at that write
        latency: std::sync::atomic::AtomicU64,
        /// micros of the player's epoch at that write
        wrote_at: std::sync::atomic::AtomicU64,
        /// the epoch every `wrote_at` value runs against
        epoch: std::time::Instant,
    }

    impl ClockShare {
        fn new() -> Self {
            Self {
                paused: false.into(),
                volume: std::sync::atomic::AtomicU32::new(256),
                audible: false.into(),
                seek_request: false.into(),
                seek_target: 0.into(),
                generation: 0.into(),
                pts_written: std::sync::atomic::AtomicI64::new(i64::MIN),
                latency: 0.into(),
                wrote_at: 0.into(),
                epoch: std::time::Instant::now(),
            }
        }

        /// The media time the sink is playing, in micros; None while
        /// muted (no sink, nothing written, or the sink died).
        fn clock_micros(&self) -> Option<i64> {
            use std::sync::atomic::Ordering::Acquire;
            if !self.audible.load(Acquire) {
                return None;
            }
            let pts = self.pts_written.load(Acquire);
            if pts == i64::MIN {
                return None;
            }
            let since = self.epoch.elapsed().as_micros() as u64;
            let since_write = since.saturating_sub(self.wrote_at.load(Acquire));
            Some(heard_micros(
                pts,
                self.latency.load(Acquire),
                since_write,
            ))
        }

        /// The decode thread's per-chunk publish: the chunk's end in
        /// media time, the latency sampled right after the write.
        fn publish(&self, end_micros: i64, latency_micros: u64) {
            use std::sync::atomic::Ordering::Release;
            self.wrote_at
                .store(self.epoch.elapsed().as_micros() as u64, Release);
            self.latency.store(latency_micros, Release);
            self.pts_written.store(end_micros, Release);
        }
    }

    /// Where decoded audio goes: the testability seam for the
    /// playback engine (the test container has no sound server). The
    /// real sink is PipeWire through pulse-simple's tiny API; the
    /// test sink records into a buffer. `write` may block, and that
    /// blocking is what paces the decode thread to realtime.
    pub(crate) trait AudioSink: Send {
        /// Interleaved S16NE samples at the stream's rate. Returns
        /// false when the sink is done for good (the sound server
        /// rejected the stream): the caller retires the audio route
        /// and the clock falls back to the wall.
        fn write(&mut self, samples: &[i16]) -> bool;
        /// Audio the sink still holds, in microseconds; None when the
        /// server would not say (the caller keeps its last reading).
        fn latency_micros(&self) -> Option<u64>;
        /// Discard buffered audio now. A pause does not: the tail
        /// drains out on its own; a seek does.
        fn flush(&mut self);
    }

    // The pulse-simple FFI, hand-rolled and dlopen'd: the build never
    // links libpulse, so a system without it still builds, and a
    // system without a sound server still plays (muted). The structs
    // are plain data from pulse's simple.h; only these six symbols
    // are ever resolved.

    /// pa_sample_spec: S16NE at the stream's rate, stereo.
    #[repr(C)]
    struct PaSampleSpec {
        format: u32,
        rate: u32,
        channels: u8,
    }
    /// PA_SAMPLE_S16NE
    const PA_SAMPLE_S16NE: u32 = 3;
    /// PA_STREAM_PLAYBACK
    const PA_STREAM_PLAYBACK: i32 = 1;

    /// pa_buffer_attr, all fields in bytes: a modest server-side
    /// buffer (see `PaSimple::open`).
    #[repr(C)]
    struct PaBufferAttr {
        maxlength: u32,
        tlength: u32,
        prebuf: u32,
        minreq: u32,
        fragsize: u32,
    }

    enum PaStream {}
    enum PaChannelMap {}

    type PaSimpleNewFn = unsafe extern "C" fn(
        *const std::ffi::c_char,
        *const std::ffi::c_char,
        i32,
        *const std::ffi::c_char,
        *const std::ffi::c_char,
        *const PaSampleSpec,
        *const PaChannelMap,
        *const PaBufferAttr,
        *mut i32,
    ) -> *mut PaStream;
    type PaSimpleWriteFn =
        unsafe extern "C" fn(*mut PaStream, *const std::ffi::c_void, libc::size_t, *mut i32) -> i32;
    type PaSimpleFlushFn = unsafe extern "C" fn(*mut PaStream, *mut i32) -> i32;
    type PaSimpleGetLatencyFn = unsafe extern "C" fn(*mut PaStream, *mut i32) -> u64;
    type PaSimpleFreeFn = unsafe extern "C" fn(*mut PaStream);

    /// The dlopen'd libpulse-simple: the resolved symbols for one
    /// stream. Never dlclosed: one mapped library for the process
    /// lifetime is cheaper than ordering every drop correctly.
    struct PaLib {
        simple_new: PaSimpleNewFn,
        simple_write: PaSimpleWriteFn,
        simple_flush: PaSimpleFlushFn,
        simple_get_latency: PaSimpleGetLatencyFn,
        simple_free: PaSimpleFreeFn,
    }

    impl PaLib {
        fn load() -> Option<PaLib> {
            let handle = unsafe { libc::dlopen(c"libpulse-simple.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                return None;
            }
            let sym = |name: &std::ffi::CStr| {
                let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
                (!p.is_null()).then_some(p)
            };
            Some(PaLib {
                simple_new: unsafe { std::mem::transmute(sym(c"pa_simple_new")?) },
                simple_write: unsafe { std::mem::transmute(sym(c"pa_simple_write")?) },
                simple_flush: unsafe { std::mem::transmute(sym(c"pa_simple_flush")?) },
                simple_get_latency: unsafe { std::mem::transmute(sym(c"pa_simple_get_latency")?) },
                simple_free: unsafe { std::mem::transmute(sym(c"pa_simple_free")?) },
            })
        }
    }

    /// A live pulse-simple playback stream: S16NE stereo at the
    /// stream's rate. The server-side buffer is capped (150ms
    /// target, 20ms granularity): a pause then leaks at most 150ms
    /// of tail, and the clock's latency readings stay tight.
    struct PaSimple {
        raw: *mut PaStream,
        simple_write: PaSimpleWriteFn,
        simple_flush: PaSimpleFlushFn,
        simple_get_latency: PaSimpleGetLatencyFn,
        simple_free: PaSimpleFreeFn,
        broken: bool,
    }

    impl PaSimple {
        fn open(lib: &PaLib, rate: u32) -> Option<PaSimple> {
            let rate = rate.max(8_000);
            let spec = PaSampleSpec {
                format: PA_SAMPLE_S16NE,
                rate,
                channels: 2,
            };
            // stereo S16: four bytes per frame of samples
            let per_sec = rate * 4;
            let minreq = per_sec * 20 / 1_000;
            let attr = PaBufferAttr {
                maxlength: u32::MAX, // the server default
                tlength: per_sec * 150 / 1_000,
                prebuf: minreq, // resume fast after an underrun
                minreq,
                fragsize: u32::MAX, // playback: unused
            };
            let raw = unsafe {
                (lib.simple_new)(
                    std::ptr::null(), // the default server
                    c"kuma-files".as_ptr(),
                    PA_STREAM_PLAYBACK,
                    std::ptr::null(), // the default device
                    c"Quick Look audio".as_ptr(),
                    &spec,
                    std::ptr::null(), // the default map for stereo
                    &attr,
                    std::ptr::null_mut(),
                )
            };
            if raw.is_null() {
                return None;
            }
            Some(PaSimple {
                raw,
                simple_write: lib.simple_write,
                simple_flush: lib.simple_flush,
                simple_get_latency: lib.simple_get_latency,
                simple_free: lib.simple_free,
                broken: false,
            })
        }
    }

    impl AudioSink for PaSimple {
        fn write(&mut self, samples: &[i16]) -> bool {
            if self.broken {
                return false;
            }
            let ok = unsafe {
                (self.simple_write)(
                    self.raw,
                    samples.as_ptr().cast(),
                    samples.len() * 2,
                    std::ptr::null_mut(),
                )
            };
            if ok < 0 {
                // say it once, then this sink is done for good: the
                // clock falls back and the rest plays silently
                self.broken = true;
                log::error!("video player: the sound server rejected audio, playing muted");
                return false;
            }
            true
        }

        fn latency_micros(&self) -> Option<u64> {
            let usec = unsafe { (self.simple_get_latency)(self.raw, std::ptr::null_mut()) };
            // (pa_usec_t)-1 is the error reading
            (usec != u64::MAX).then_some(usec)
        }

        fn flush(&mut self) {
            unsafe { (self.simple_flush)(self.raw, std::ptr::null_mut()) };
        }
    }

    impl Drop for PaSimple {
        fn drop(&mut self) {
            unsafe { (self.simple_free)(self.raw) };
        }
    }

    // The stream lives and dies on the decode thread that owns it;
    // pulse's rule is only that no two threads touch one stream at
    // once, the same reasoning as `FormatInput`.
    unsafe impl Send for PaSimple {}

    /// RAII for the resampler.
    struct Swr(*mut sys::SwrContext);

    impl Drop for Swr {
        fn drop(&mut self) {
            unsafe { sys::swr_free(&mut self.0) };
        }
    }

    /// One decoded audio frame into interleaved S16NE stereo samples
    /// at the stream's rate: the sink's fixed format. The resampler
    /// is created on the first frame (its layout and rate pin the
    /// input side; the output is stereo at the same rate, so the
    /// common case is a pure format conversion and any channel count
    /// downmixes). The volume rides as an integer gain, 256 = unity.
    /// Returns the samples per channel converted.
    unsafe fn audio_frame_to_s16(
        frame: *mut sys::AVFrame,
        swr: &mut Option<Swr>,
        out: &mut Vec<i16>,
        gain: u32,
    ) -> Result<i32, PosterFail> {
        unsafe {
            let nb = (*frame).nb_samples;
            if nb <= 0 {
                return Ok(0);
            }
            if swr.is_none() {
                let mut out_layout: sys::AVChannelLayout = std::mem::zeroed();
                sys::av_channel_layout_default(&mut out_layout, 2);
                let mut raw: *mut sys::SwrContext = std::ptr::null_mut();
                if sys::swr_alloc_set_opts2(
                    &mut raw,
                    &out_layout,
                    sys::AVSampleFormat::AV_SAMPLE_FMT_S16,
                    (*frame).sample_rate,
                    std::ptr::addr_of!((*frame).ch_layout),
                    // the format came from libav itself, its enum
                    // value is valid by construction
                    std::mem::transmute::<i32, sys::AVSampleFormat>((*frame).format),
                    (*frame).sample_rate,
                    0,
                    std::ptr::null_mut(),
                ) < 0
                {
                    return Err(PosterFail::Empty);
                }
                let ctx = Swr(raw);
                if sys::swr_init(ctx.0) < 0 {
                    return Err(PosterFail::Empty);
                }
                *swr = Some(ctx);
            }
            let ctx = swr.as_ref().unwrap().0;
            // stereo packed S16: two i16 per frame of samples
            out.clear();
            out.reserve(nb as usize * 2);
            let mut out_planes = [out.as_mut_ptr().cast::<u8>()];
            let got = sys::swr_convert(
                ctx,
                out_planes.as_mut_ptr(),
                nb,
                std::ptr::addr_of!((*frame).data).cast(),
                nb,
            );
            if got < 0 {
                return Err(PosterFail::Empty);
            }
            out.set_len(got as usize * 2);
            if gain != 256 {
                for sample in out.iter_mut() {
                    *sample = (((*sample as i32) * gain as i32) >> 8)
                        .clamp(i16::MIN as i32, i16::MAX as i32)
                        as i16;
                }
            }
            Ok(got)
        }
    }

    /// Open the best audio stream's decoder, best effort: None when
    /// the file has no audio stream or this system's libav lacks the
    /// decoder (the muted mode, never a failure).
    unsafe fn open_audio_decoder(
        input: *mut sys::AVFormatContext,
    ) -> Option<(usize, CodecCtx, u32)> {
        unsafe {
            let found = sys::av_find_best_stream(
                input,
                sys::AVMediaType::AVMEDIA_TYPE_AUDIO,
                -1,
                -1,
                std::ptr::null_mut(),
                0,
            );
            if found < 0 {
                return None;
            }
            let index = found as usize;
            let streams = *std::ptr::addr_of!((*input).streams);
            let stream = *streams.add(index);
            let par = (*stream).codecpar;
            if decoder_missing((*par).codec_id) || (*par).sample_rate == 0 {
                return None;
            }
            let codec = sys::avcodec_find_decoder((*par).codec_id);
            let actx = avcodec_open(codec, par).ok()?;
            Some((index, actx, (*par).sample_rate as u32))
        }
    }

    /// One playing video: the pane's handle on the decode thread.
    /// The thread owns libav and the sink end to end, decodes ahead
    /// into the bounded ring, and blocks when the ring is full: the
    /// backpressure is the pause, the drain is the play. Dropping
    /// the `Player` drops the receiver, which fails a thread's next
    /// send and wakes a blocked one: no stop flag, no join.
    pub(crate) struct Player {
        rx: std::sync::mpsc::Receiver<PlayerMsg>,
        share: std::sync::Arc<ClockShare>,
        /// the container's duration in seconds, from the header at
        /// open; None when the container states none
        duration: Option<f64>,
    }

    impl Player {
        /// The container's duration, when the header stated one: the
        /// seek bar's total and the seek keys' clamp.
        pub(crate) fn duration_secs(&self) -> Option<f64> {
            self.duration.filter(|secs| *secs > 0.0)
        }

        /// Seek to a media time in seconds: the loop jumps the
        /// demuxer, flushes both decoders and the sink, and bumps the
        /// generation; frames decoded after carry the new one. The
        /// target clamps at zero; the container's end is the caller's
        /// business.
        pub(crate) fn request_seek(&self, target_secs: f64) {
            self.share
                .seek_target
                .store((target_secs.max(0.0) * 1_000_000.0) as i64, Release);
            self.share.seek_request.store(true, Release);
        }

        /// The current seek generation: the pane records it at
        /// request time and drops frames at or before it.
        pub(crate) fn generation(&self) -> u32 {
            self.share.generation.load(Acquire)
        }

        /// Open the container and spawn the decode thread. The open
        /// itself (header parse plus stream info) costs libav tens
        /// of milliseconds: run it off the UI thread like every
        /// other decode. `Err` names the failure exactly where the
        /// poster path does. Decodes at `PLAYER_MAX`. Sound is best
        /// effort: no audio stream, no pulse-simple library, or no
        /// sound server all mean the muted mode (video on the wall
        /// clock), which is a degradation, not a failure.
        pub(crate) fn open(path: &Path) -> Result<Player, PosterFail> {
            Self::spawn(path, None)
        }

        /// The test seam: the same open with a forced sink, so the
        /// audio path runs in a container with no sound server.
        #[cfg(test)]
        pub(crate) fn open_with_sink(
            path: &Path,
            sink: Box<dyn AudioSink>,
        ) -> Result<Player, PosterFail> {
            Self::spawn(path, Some(sink))
        }

        fn spawn(path: &Path, forced: Option<Box<dyn AudioSink>>) -> Result<Player, PosterFail> {
            let cpath = cpath(path).ok_or(PosterFail::Empty)?;
            let (input, video_index, vctx) = unsafe { open_video_decoder(&cpath)? };
            // the header's duration (micros): the seek bar's total.
            // AV_NOPTS_VALUE reads negative, which the filter takes
            let duration = unsafe {
                ((*input.0).duration >= 0)
                    .then(|| (*input.0).duration as f64 / 1_000_000.0)
            };
            let audio = unsafe { open_audio_decoder(input.0) };
            // the sink: the forced one (tests) wins; otherwise
            // pulse-simple, best effort. Anything missing means
            // muted, and then the audio is not decoded at all.
            let sink = forced.or_else(|| {
                let rate = audio.as_ref()?.2;
                let lib = PaLib::load()?;
                PaSimple::open(&lib, rate).map(|s| Box::new(s) as Box<dyn AudioSink>)
            });
            // with nowhere to put the sound, the audio is not
            // decoded at all: the muted mode runs on the wall clock
            let audio = if sink.is_some() { audio } else { None };
            let (audio_index, actx) = match audio {
                Some((index, actx, _)) => (Some(index), Some(actx)),
                None => (None, None),
            };
            let share = std::sync::Arc::new(ClockShare::new());
            share
                .audible
                .store(sink.is_some(), std::sync::atomic::Ordering::Release);
            let (tx, rx) = std::sync::mpsc::sync_channel(PLAYER_RING);
            let thread_share = share.clone();
            let spawned = std::thread::Builder::new()
                .name("kuma-video-player".into())
                .spawn(move || {
                    // hostile input: a panic in libav-land ends the
                    // thread, never the app; the pane reads the dead
                    // channel as eof
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                        play_loop(
                            input,
                            video_index,
                            vctx,
                            audio_index,
                            actx,
                            sink,
                            thread_share,
                            tx,
                            PLAYER_MAX,
                        );
                    }));
                });
            spawned.map_err(|_| PosterFail::Empty)?;
            Ok(Player { rx, share, duration })
        }

        /// The next message if one is ready: a frame, the stream's
        /// end, or nothing yet (the ring is empty and the thread
        /// still runs). Never blocks, so it is safe on the UI
        /// thread: the libav work stays on the thread that owns it.
        pub(crate) fn try_recv(&self) -> Option<PlayerMsg> {
            self.rx.try_recv().ok()
        }

        /// The media time the sound server is playing, in micros;
        /// None while muted. Lock-free, safe on the UI thread.
        pub(crate) fn clock_micros(&self) -> Option<i64> {
            self.share.clock_micros()
        }

        /// Pause or resume. While paused the audio route skips its
        /// packets (silence, the clock sits still) and the video
        /// coasts until the ring fills; the sink's bounded tail
        /// drains out on its own.
        pub(crate) fn set_paused(&self, paused: bool) {
            self.share
                .paused
                .store(paused, std::sync::atomic::Ordering::Release);
        }

        /// Integer gain, 256 = unity, applied in the S16 conversion.
        pub(crate) fn set_volume(&self, gain: u32) {
            self.share
                .volume
                .store(gain.min(256), std::sync::atomic::Ordering::Release);
        }
    }

    /// The player's decode loop, on the decode thread: read, route
    /// by stream, decode, ship. Audio packets decode into the sink
    /// (whose write blocks when the server's buffer is full: that
    /// blocking is the realtime pace) and publish the audio clock;
    /// video packets decode into the ring. Ends at eof, a decode
    /// error, or a gone pane (the send fails once the receiver
    /// dropped; a send blocked on the full ring wakes the same way).
    /// `tx` ships the terminal `Eof` so the pane parks cleanly.
    #[allow(clippy::too_many_arguments)]
    unsafe fn play_loop(
        input: FormatInput,
        video_index: usize,
        vctx: CodecCtx,
        audio_index: Option<usize>,
        actx: Option<CodecCtx>,
        mut sink: Option<Box<dyn AudioSink>>,
        share: std::sync::Arc<ClockShare>,
        tx: std::sync::mpsc::SyncSender<PlayerMsg>,
        max: u32,
    ) {
        unsafe {
            let Some(pkt) = Packet::alloc() else {
                return;
            };
            let Some(frame) = Frame::alloc() else {
                return;
            };
            let streams = *std::ptr::addr_of!((*input.0).streams);
            let vstream = *streams.add(video_index);
            let v_tb = (*vstream).time_base;
            // the synthetic clock for streams without usable
            // timestamps: the stream's average frame rate, 25fps
            // when unstated
            let rate = (*vctx.0).framerate;
            let frame_dur = if rate.num > 0 {
                rate.den as f64 / rate.num as f64
            } else {
                1.0 / 25.0
            };
            let mut fallback: f64 = 0.0;
            let mut scaler: Option<Scaler> = None;
            // the audio side travels together: stream index, decoder,
            // the stream's time base, and the stream's rate
            let audio = match (audio_index, actx) {
                (Some(index), Some(actx)) => {
                    let a_tb = (*(*streams.add(index))).time_base;
                    let a_rate = (*actx.0).sample_rate.max(1) as u32;
                    Some((index, actx, a_tb, a_rate))
                }
                _ => None,
            };
            let mut swr: Option<Swr> = None;
            let mut samples: Vec<i16> = Vec::new();
            let mut last_latency: u64 = 0;
            'read: loop {
                // the pane's seek: jump the demuxer, flush both
                // decoders and the sink, and bump the generation.
                // Frames decoded after carry the new one and the pane
                // drops everything older, so the ring's stale frames
                // never show. A failed jump still flushes and still
                // bumps: the stream plays on from where it was, and
                // the pane's scrub window closes instead of hanging.
                if share.seek_request.swap(false, Acquire) {
                    let target = share.seek_target.load(Acquire);
                    // stream -1 seeks the container in AV_TIME_BASE
                    // micros, the poster path's shape
                    let _ = sys::avformat_seek_file(
                        input.0,
                        -1,
                        i64::MIN,
                        target,
                        i64::MAX,
                        0,
                    );
                    sys::avcodec_flush_buffers(vctx.0);
                    if let Some((_, actx, _, _)) = &audio {
                        sys::avcodec_flush_buffers(actx.0);
                    }
                    if let Some(sink) = sink.as_mut() {
                        sink.flush();
                    }
                    share.generation.fetch_add(1, Release);
                }
                // the pane's pause: sound off, picture coasting. Video
                // keeps decoding until the ring fills and the send
                // parks the thread; the receiver's drop wakes a
                // send-blocked thread, so no park loop is needed. The
                // resume continues at the audio's own position, so
                // sound and picture stay locked (the picture jumps
                // the tail's worth, at most a ring length).
                let paused = share.paused.load(Acquire);
                if sys::av_read_frame(input.0, pkt.0) < 0 {
                    break; // eof or read error: the stream is what it is
                }
                let index = (*pkt.0).stream_index as usize;
                let routed_audio = audio.as_ref().is_some_and(|a| a.0 == index);
                if !routed_audio && index != video_index {
                    sys::av_packet_unref(pkt.0);
                    continue;
                }
                // a paused pane hears nothing: audio packets skip,
                // and the clock sits still until the resume
                if paused && routed_audio {
                    sys::av_packet_unref(pkt.0);
                    continue;
                }
                // one packet's decode: audio into the sink, video
                // into the ring. `stop` ends the stream, `muted`
                // retires the audio route only.
                let mut stop = false;
                let mut muted = false;
                if routed_audio {
                    // paired at open; unroutable audio is not a
                    // panic, just a skip
                    let Some((_, actx, a_tb, a_rate)) = &audio else {
                        sys::av_packet_unref(pkt.0);
                        continue;
                    };
                    let Some(sink) = sink.as_mut() else {
                        sys::av_packet_unref(pkt.0);
                        continue;
                    };
                    if sys::avcodec_send_packet(actx.0, pkt.0) < 0 {
                        stop = true;
                    } else {
                        loop {
                            let got = sys::avcodec_receive_frame(actx.0, frame.0);
                            if got < 0 {
                                break; // EAGAIN or error: next packet
                            }
                            let count = match audio_frame_to_s16(
                                frame.0,
                                &mut swr,
                                &mut samples,
                                share.volume.load(std::sync::atomic::Ordering::Acquire),
                            ) {
                                Ok(ok) => ok,
                                Err(_) => {
                                    stop = true;
                                    break;
                                }
                            };
                            let pts =
                                frame_pts_seconds(frame.0, *a_tb, actx).unwrap_or(fallback);
                            // the chunk's end in media time: its start
                            // plus the samples it holds
                            let end_micros =
                                ((pts + count as f64 / *a_rate as f64) * 1_000_000.0) as i64;
                            sys::av_frame_unref(frame.0);
                            if !sink.write(&samples) {
                                // the sink is done for good: keep the
                                // video going on the wall clock
                                muted = true;
                                break;
                            }
                            if let Some(lat) = sink.latency_micros() {
                                last_latency = lat;
                            }
                            share.publish(end_micros, last_latency);
                        }
                    }
                } else {
                    if sys::avcodec_send_packet(vctx.0, pkt.0) < 0 {
                        stop = true;
                    } else {
                        loop {
                            let got = sys::avcodec_receive_frame(vctx.0, frame.0);
                            if got < 0 {
                                break; // EAGAIN or error: next packet
                            }
                            let (dw, dh, mut buf) =
                                match scale_frame(frame.0, &mut scaler, max) {
                                    Ok(ok) => ok,
                                    Err(_) => {
                                        stop = true;
                                        break;
                                    }
                                };
                            let pts = frame_pts_seconds(frame.0, v_tb, &vctx)
                                .unwrap_or(fallback);
                            fallback = pts + frame_dur;
                            sys::av_frame_unref(frame.0);
                            // gpui's atlas wants BGRA
                            for pixel in buf.chunks_exact_mut(4) {
                                pixel.swap(0, 2);
                            }
                            let Some(rgba) = image::RgbaImage::from_raw(dw, dh, buf) else {
                                stop = true;
                                break;
                            };
                            let image = gpui::RenderImage::new(smallvec::SmallVec::from_buf([
                                image::Frame::new(rgba),
                            ]));
                            let ship = tx.send(PlayerMsg::Frame {
                                image: std::sync::Arc::new(image),
                                pts,
                                generation: share.generation.load(Acquire),
                            });
                            if ship.is_err() {
                                return; // the pane moved on: the job is done
                            }
                        }
                    }
                }
                if muted {
                    let _ = sink.take();
                    share
                        .audible
                        .store(false, std::sync::atomic::Ordering::Release);
                }
                sys::av_packet_unref(pkt.0);
                if stop {
                    break 'read;
                }
            }
            let _ = tx.send(PlayerMsg::Eof);
        }
    }

    /// The frame's presentation time in seconds. libav stamps the
    /// frame's own time base when it has one; the container's stream
    /// time base is what the copied-through packet timestamps run in
    /// otherwise, and the codec context's is the last resort. None
    /// when the timestamp is unknown (the caller synthesizes).
    unsafe fn frame_pts_seconds(
        frame: *mut sys::AVFrame,
        stream_tb: sys::AVRational,
        cctx: &CodecCtx,
    ) -> Option<f64> {
        unsafe {
            let tb = if (*frame).time_base.num > 0 {
                (*frame).time_base
            } else if stream_tb.num > 0 {
                stream_tb
            } else {
                (*cctx.0).time_base
            };
            ((*frame).pts >= 0 && tb.num > 0).then(|| {
                sys::av_rescale_q(
                    (*frame).pts,
                    tb,
                    sys::AVRational {
                        num: 1,
                        den: 1_000_000,
                    },
                ) as f64
                    / 1_000_000.0
            })
        }
    }

    /// The poster's dynamic twin: the fitted RGBA pixels as a plain
    /// `DynamicImage`, pre-swap, for the grid thumb job's disk cache.
    pub(crate) fn poster_dynamic(
        path: &Path,
        max: u32,
    ) -> Result<image::DynamicImage, PosterFail> {
        let path = cpath(path).ok_or(PosterFail::Empty)?;
        poster(&path, max, false).or_else(|_| poster(&path, max, true))
    }

    fn poster(
        path: &CString,
        max: u32,
        from_head: bool,
    ) -> Result<image::DynamicImage, PosterFail> {
        unsafe {
            let (input, video_index, mut cctx) = open_video_decoder(path)?;
            if !from_head {
                // the seek's timestamp runs in the container time
                // base, microseconds; ten percent in, one second as
                // the ceiling
                let raw = (*input.0).duration;
                let tenth = (raw > 0)
                    .then(|| (raw as f64 * 0.1) as i64)
                    .unwrap_or(1_000_000);
                if sys::avformat_seek_file(
                    input.0,
                    -1,
                    i64::MIN,
                    tenth.min(1_000_000),
                    i64::MAX,
                    0,
                ) < 0
                {
                    return Err(PosterFail::Empty);
                }
            }
            decode_first_frame(input.0, &mut cctx, video_index, max)
        }
    }

    /// The clip's head as fitted RGBA frames: at most the preview
    /// budget's frames, span and bytes, per-frame delays from PTS
    /// deltas. Starts at the head because playback is honest:
    /// recordings that start black play past it. `Err(CodecMissing)`
    /// propagates from the decoder check; a stream that yields
    /// nothing decodable is `Err(Empty)` (the caller falls back to
    /// the poster).
    fn preview(path: &CString, max: u32) -> Result<Vec<image::Frame>, PosterFail> {
        unsafe {
            let (input, video_index, cctx) = open_video_decoder(path)?;
            let pkt = Packet::alloc().ok_or(PosterFail::Empty)?;
            let frame = Frame::alloc().ok_or(PosterFail::Empty)?;
            // the container's own time base for the stream: the
            // decoder copies packet pts through unchanged, so this
            // is what the frames' timestamps actually run in when
            // libav does not stamp a frame time base
            let streams = *std::ptr::addr_of!((*input.0).streams);
            let stream = *streams.add(video_index);
            let stream_tb = (*stream).time_base;
            let mut scaler: Option<Scaler> = None;
            let mut collected: Vec<(image::RgbaImage, Option<i64>)> = Vec::new();
            let mut bytes_total = 0usize;
            let mut first_pts: Option<i64> = None;
            'collect: loop {
                if sys::av_read_frame(input.0, pkt.0) < 0 {
                    break; // eof or read error: the loop is what it is
                }
                if (*pkt.0).stream_index != video_index as i32 {
                    sys::av_packet_unref(pkt.0);
                    continue;
                }
                if sys::avcodec_send_packet(cctx.0, pkt.0) < 0 {
                    sys::av_packet_unref(pkt.0);
                    break;
                }
                loop {
                    let got = sys::avcodec_receive_frame(cctx.0, frame.0);
                    if got < 0 {
                        break; // EAGAIN or error: next packet
                    }
                    let (dw, dh, buf) = scale_frame(frame.0, &mut scaler, max)?;
                    // frame pts: libav stamps the frame's own time
                    // base when it has one; the container's stream
                    // time base is what the copied-through packet
                    // timestamps run in otherwise
                    let tb = if (*frame.0).time_base.num > 0 {
                        (*frame.0).time_base
                    } else if stream_tb.num > 0 {
                        stream_tb
                    } else {
                        (*cctx.0).time_base
                    };
                    let pts = ((*frame.0).pts >= 0 && tb.num > 0).then(|| {
                        sys::av_rescale_q(
                            (*frame.0).pts,
                            tb,
                            sys::AVRational {
                                num: 1,
                                den: 1_000_000,
                            },
                        )
                    });
                    bytes_total += buf.len();
                    collected.push((
                        image::RgbaImage::from_raw(dw, dh, buf).ok_or(PosterFail::Empty)?,
                        pts,
                    ));
                    sys::av_frame_unref(frame.0);
                    let spanned = pts
                        .zip(first_pts)
                        .is_some_and(|(p, first)| p - first > (PREVIEW_MAX_SECS * 1e6) as i64);
                    first_pts = first_pts.or(pts);
                    if collected.len() >= PREVIEW_MAX_FRAMES
                        || bytes_total >= PREVIEW_MAX_BYTES
                        || spanned
                    {
                        break 'collect;
                    }
                }
            }
            // frame i displays for the gap to the next frame's
            // arrival; the last frame wraps on the previous gap
            let n = collected.len();
            let mut delays = vec![PREVIEW_MIN_DELAY; n];
            for ix in 0..n.saturating_sub(1) {
                delays[ix] = delay_from_pts(collected[ix].1, collected[ix + 1].1, PREVIEW_MIN_DELAY);
            }
            if n > 1 {
                delays[n - 1] =
                    delay_from_pts(collected[n - 2].1, collected[n - 1].1, PREVIEW_MIN_DELAY);
            }
            Ok(collected
                .into_iter()
                .enumerate()
                .map(|(ix, (mut buffer, _))| {
                    // gpui's atlas wants BGRA
                    for pixel in buffer.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                    }
                    image::Frame::from_parts(
                        buffer,
                        0,
                        0,
                        image::Delay::from_saturating_duration(std::mem::take(&mut delays[ix])),
                    )
                })
                .collect())
        }
    }

    /// Open the container, find the best video stream, open its
    /// decoder. `Err(CodecMissing)` when libav parses the container
    /// but has no decoder for the codec (the named state's trigger).
    unsafe fn open_video_decoder(
        path: &CString,
    ) -> Result<(FormatInput, usize, CodecCtx), PosterFail> {
        unsafe {
            let mut ic: *mut sys::AVFormatContext = std::ptr::null_mut();
            if sys::avformat_open_input(
                &mut ic,
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
            ) < 0
            {
                return Err(PosterFail::Empty);
            }
            let input = FormatInput(ic);
            if sys::avformat_find_stream_info(input.0, std::ptr::null_mut()) < 0 {
                return Err(PosterFail::Empty);
            }
            let (video_index, par) = best_video_stream(input.0).ok_or(PosterFail::Empty)?;
            // the named state: the container parses, but this
            // system's libav has no decoder for the codec
            if decoder_missing((*par).codec_id) {
                return Err(PosterFail::CodecMissing);
            }
            let codec = sys::avcodec_find_decoder((*par).codec_id);
            let cctx = avcodec_open(codec, par)?;
            Ok((input, video_index, cctx))
        }
    }

    unsafe fn avcodec_open(
        codec: *const sys::AVCodec,
        par: *mut sys::AVCodecParameters,
    ) -> Result<CodecCtx, PosterFail> {
        unsafe {
            let cctx = sys::avcodec_alloc_context3(codec);
            if cctx.is_null() {
                return Err(PosterFail::Empty);
            }
            let cctx = CodecCtx(cctx);
            if sys::avcodec_parameters_to_context(cctx.0, par) < 0
                || sys::avcodec_open2(cctx.0, codec, std::ptr::null_mut()) < 0
            {
                return Err(PosterFail::Empty);
            }
            Ok(cctx)
        }
    }

    /// Packets in, first decodable frame out, fitted in the scaler
    /// so no full-size RGBA intermediate exists.
    unsafe fn decode_first_frame(
        ic: *mut sys::AVFormatContext,
        cctx: &mut CodecCtx,
        video_index: usize,
        max: u32,
    ) -> Result<image::DynamicImage, PosterFail> {
        unsafe {
            let pkt = Packet::alloc().ok_or(PosterFail::Empty)?;
            let frame = Frame::alloc().ok_or(PosterFail::Empty)?;
            let mut scaler: Option<Scaler> = None;
            loop {
                if sys::av_read_frame(ic, pkt.0) < 0 {
                    return Err(PosterFail::Empty); // eof or read error: no frame
                }
                if (*pkt.0).stream_index != video_index as i32 {
                    sys::av_packet_unref(pkt.0);
                    continue;
                }
                if sys::avcodec_send_packet(cctx.0, pkt.0) < 0 {
                    return Err(PosterFail::Empty);
                }
                loop {
                    let got = sys::avcodec_receive_frame(cctx.0, frame.0);
                    if got < 0 {
                        break; // EAGAIN or error: next packet
                    }
                    let (dw, dh, buf) = scale_frame(frame.0, &mut scaler, max)?;
                    sys::av_frame_unref(frame.0);
                    let image =
                        image::RgbaImage::from_raw(dw, dh, buf).ok_or(PosterFail::Empty)?;
                    return Ok(image::DynamicImage::ImageRgba8(image));
                }
                sys::av_packet_unref(pkt.0);
            }
        }
    }

    /// One decoded AVFrame into fitted RGBA bytes: the scaler is
    /// created on the first frame (its format pins the source pixel
    /// format), the downscale happens in the scaler so no full-size
    /// RGBA intermediate exists.
    unsafe fn scale_frame(
        frame: *mut sys::AVFrame,
        scaler: &mut Option<Scaler>,
        max: u32,
    ) -> Result<(u32, u32, Vec<u8>), PosterFail> {
        unsafe {
            if scaler.is_none() {
                let (dw, dh) = fit((*frame).width as u32, (*frame).height as u32, max);
                let sc = sys::sws_getContext(
                    (*frame).width,
                    (*frame).height,
                    // the format came from libav itself, its
                    // enum value is valid by construction
                    std::mem::transmute::<i32, sys::AVPixelFormat>((*frame).format),
                    dw as i32,
                    dh as i32,
                    sys::AVPixelFormat::AV_PIX_FMT_RGBA,
                    sys::SwsFlags::SWS_BILINEAR as i32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if sc.is_null() {
                    return Err(PosterFail::Empty);
                }
                *scaler = Some(Scaler(sc));
            }
            let sc = scaler.as_ref().unwrap().0;
            let (dw, dh) = fit((*frame).width as u32, (*frame).height as u32, max);
            let mut buf = vec![0u8; dw as usize * dh as usize * 4];
            let src_data = (*frame).data.as_ptr() as *const *const u8;
            let src_stride = (*frame).linesize.as_ptr();
            let mut dst_data = [
                buf.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ];
            let dst_stride = [(dw as usize * 4) as i32, 0, 0, 0];
            if sys::sws_scale(
                sc,
                src_data,
                src_stride,
                0,
                (*frame).height,
                dst_data.as_mut_ptr() as *const *mut u8,
                dst_stride.as_ptr(),
            ) < 0
            {
                return Err(PosterFail::Empty);
            }
            Ok((dw, dh, buf))
        }
    }

    /// The frame fitted into a box of `max` on the long edge, never
    /// upscaled: the downscale happens in the scaler so no full-size
    /// RGBA intermediate exists.
    fn fit(w: u32, h: u32, max: u32) -> (u32, u32) {
        let (w, h) = (w as f64, h as f64);
        let scale = (max as f64 / w).min(max as f64 / h).min(1.0);
        let dw = ((w * scale).round() as u32).max(1);
        let dh = ((h * scale).round() as u32).max(1);
        (dw, dh)
    }
}

#[cfg(feature = "video")]
pub(crate) use imp::{Player, PlayerMsg, decode_poster, decode_preview, poster_dynamic, probe};

#[cfg(not(feature = "video"))]
mod imp {
    //! The portability stub: video decode is a capability, not a
    //! requirement. Without libav the app builds and runs anywhere,
    //! and video is a degraded feature: the type icon and the plain
    //! card stand in, the panes stay up.
    use super::PosterFail;
    use std::path::Path;

    pub(crate) fn probe(_path: &Path) -> Option<(f64, u32, u32)> {
        None
    }

    /// The degraded path always errs, and the panes stand in the
    /// card: the QL pane's poster beat and the tests assert exactly
    /// that.
    pub(crate) fn decode_poster(
        _path: &Path,
        _max: u32,
    ) -> Result<gpui::RenderImage, PosterFail> {
        Err(PosterFail::Empty)
    }

    pub(crate) fn decode_preview(
        _path: &Path,
        _max: u32,
    ) -> Result<gpui::RenderImage, PosterFail> {
        Err(PosterFail::Empty)
    }

    /// The playback engine's stub: opening a player always errs, so
    /// the pane's named card stands in and no thread ever spawns.
    pub(crate) struct Player;

    impl Player {
        pub(crate) fn open(_path: &Path) -> Result<Player, PosterFail> {
            Err(PosterFail::Empty)
        }

        pub(crate) fn try_recv(&self) -> Option<PlayerMsg> {
            None
        }

        pub(crate) fn clock_micros(&self) -> Option<i64> {
            None
        }

        pub(crate) fn set_paused(&self, _paused: bool) {}

        pub(crate) fn set_volume(&self, _gain: u32) {}

        pub(crate) fn duration_secs(&self) -> Option<f64> {
            None
        }

        pub(crate) fn request_seek(&self, _target_secs: f64) {}

        pub(crate) fn generation(&self) -> u32 {
            0
        }
    }

    /// The pane matches on these variants, the stub never builds
    /// them (nothing decodes), hence the allow.
    #[allow(dead_code)]
    pub(crate) enum PlayerMsg {
        Frame {
            image: std::sync::Arc<gpui::RenderImage>,
            pts: f64,
            generation: u32,
        },
        Eof,
    }

    pub(crate) fn poster_dynamic(
        _path: &Path,
        _max: u32,
    ) -> Result<image::DynamicImage, PosterFail> {
        Err(PosterFail::Empty)
    }
}

#[cfg(not(feature = "video"))]
pub(crate) use imp::{Player, PlayerMsg, decode_poster, decode_preview, poster_dynamic, probe};

/// Test helper (the `video` feature on, test builds only): a raw
/// H.264 elementary stream whose container parses but whose codec
/// the free build cannot decode, so the panes exercise the named
/// codec-missing state deterministically.
#[cfg(all(test, feature = "video"))]
pub(crate) fn write_codec_missing_stream(path: &std::path::Path) {
    use std::fs;
    fs::write(
        path,
        [
            0u8, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, //
            0, 0, 0, 1, 0x68, 0xeb, 0xec, 0xb2, 0x2c, //
            0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x21, 0xe0,
        ],
    )
    .unwrap();
}

#[cfg(all(test, feature = "video"))]
mod tests {
    use super::imp::{
        AudioSink, Player, PlayerMsg, decode_poster, decode_preview, heard_micros,
        poster_dynamic, probe,
    };
    use super::{delay_from_pts, duration_label};
    use super::PosterFail;
    use super::write_codec_missing_stream;
    use ffmpeg_sys_next as sys;
    use std::ffi::CString;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    /// One-time fixture generator: a one-second 64x64 mpeg4 with
    /// moving luma bars and a flac audio track (440Hz stereo at
    /// 44100, the free build's audio encoder), committed as
    /// tests/fixtures/sample.mp4 so the decode tests run headless
    /// with no GPU and no encoder at test time; its ten-second twin
    /// (video only) pins the preview budget. Run with `--ignored` to
    /// regenerate.
    #[test]
    #[ignore]
    fn generate_fixture() {
        encode_fixture(Path::new("tests/fixtures/sample.mp4"), 25, true);
    }

    #[test]
    #[ignore]
    fn generate_long_fixture() {
        encode_fixture(Path::new("tests/fixtures/sample-long.mp4"), 250, false);
    }

    fn encode_fixture(path: &Path, frames: i64, audio: bool) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let cpath = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        unsafe {
            let mut octx: *mut sys::AVFormatContext = std::ptr::null_mut();
            assert_eq!(
                sys::avformat_alloc_output_context2(
                    &mut octx,
                    std::ptr::null(),
                    std::ptr::null(),
                    cpath.as_ptr(),
                ),
                0
            );
            let codec = sys::avcodec_find_encoder(sys::AVCodecID::AV_CODEC_ID_MPEG4);
            assert!(!codec.is_null(), "the free build has the mpeg4 encoder");
            let ost = sys::avformat_new_stream(octx, codec);
            assert!(!ost.is_null());
            let mut enc = sys::avcodec_alloc_context3(codec);
            assert!(!enc.is_null());
            (*enc).width = 64;
            (*enc).height = 64;
            (*enc).pix_fmt = sys::AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*enc).time_base = sys::AVRational { num: 1, den: 25 };
            (*enc).framerate = sys::AVRational { num: 25, den: 1 };
            assert_eq!(sys::avcodec_open2(enc, codec, std::ptr::null_mut()), 0);
            assert_eq!(
                sys::avcodec_parameters_from_context((*ost).codecpar, enc),
                0
            );
            (*ost).time_base = sys::AVRational { num: 1, den: 25 };

            // the audio stream, when wanted: one second of 440Hz
            // stereo, encoded flac
            let mut aenc: *mut sys::AVCodecContext = std::ptr::null_mut();
            if audio {
                let codec = sys::avcodec_find_encoder(sys::AVCodecID::AV_CODEC_ID_FLAC);
                assert!(!codec.is_null(), "the free build has the flac encoder");
                let ast = sys::avformat_new_stream(octx, codec);
                assert!(!ast.is_null());
                aenc = sys::avcodec_alloc_context3(codec);
                assert!(!aenc.is_null());
                // this encoder takes s16 packed (its log says so)
                (*aenc).sample_fmt = sys::AVSampleFormat::AV_SAMPLE_FMT_S16;
                (*aenc).sample_rate = 44100;
                sys::av_channel_layout_default(&mut (*aenc).ch_layout, 2);
                (*aenc).time_base = sys::AVRational { num: 1, den: 44100 };
                assert_eq!(sys::avcodec_open2(aenc, codec, std::ptr::null_mut()), 0);
                assert_eq!(
                    sys::avcodec_parameters_from_context((*ast).codecpar, aenc),
                    0
                );
                (*ast).time_base = sys::AVRational { num: 1, den: 44100 };
            }

            assert_eq!(
                sys::avio_open(
                    &mut (*octx).pb as *mut *mut sys::AVIOContext,
                    cpath.as_ptr(),
                    sys::AVIO_FLAG_WRITE,
                ),
                0
            );
            assert_eq!(sys::avformat_write_header(octx, std::ptr::null_mut()), 0);
            let src_tb = sys::AVRational { num: 1, den: 25 };
            let dst_tb = (**(*octx).streams.add(0)).time_base;

            let mut frame = sys::av_frame_alloc();
            assert!(!frame.is_null());
            (*frame).format = sys::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            (*frame).width = 64;
            (*frame).height = 64;
            assert_eq!(sys::av_frame_get_buffer(frame, 0), 0);

            let mut pkt = sys::av_packet_alloc();
            assert!(!pkt.is_null());
            let send = |pts: i64| {
                (*frame).pts = pts;
                assert_eq!(sys::avcodec_send_frame(enc, if pts < 0 { std::ptr::null_mut() } else { frame }), 0);
                loop {
                    let got = sys::avcodec_receive_packet(enc, pkt);
                    if got < 0 {
                        break; // EAGAIN or eof
                    }
                    (*pkt).stream_index = 0;
                    sys::av_packet_rescale_ts(pkt, src_tb, dst_tb);
                    assert_eq!(sys::av_interleaved_write_frame(octx, pkt), 0);
                }
            };
            for i in 0i64..frames {
                // moving luma bars: the poster test asserts real content
                let stride = (*frame).linesize[0] as usize;
                let y = (*frame).data[0] as *mut u8;
                for row in 0..64usize {
                    for col in 0..64usize {
                        *y.add(row * stride + col) = ((row + col + i as usize * 8) % 256) as u8;
                    }
                }
                for plane in 1..3 {
                    let stride = (*frame).linesize[plane] as usize;
                    let chroma = (*frame).data[plane] as *mut u8;
                    for cell in 0..32 * 32usize {
                        *chroma.add(cell % 32 + cell / 32 * stride) = 128;
                    }
                }
                send(i);
            }
            send(-1); // eof: flush the encoder

            if audio {
                // the audio track: 44100 samples of 440Hz sine in
                // 1024-sample blocks, s16 packed (the flac encoder's
                // format)
                let mut aframe = sys::av_frame_alloc();
                assert!(!aframe.is_null());
                (*aframe).format = sys::AVSampleFormat::AV_SAMPLE_FMT_S16 as i32;
                (*aframe).sample_rate = 44100;
                (*aframe).nb_samples = (*aenc).frame_size.max(1);
                sys::av_channel_layout_default(&mut (*aframe).ch_layout, 2);
                assert_eq!(sys::av_frame_get_buffer(aframe, 0), 0);
                let mut apkt = sys::av_packet_alloc();
                assert!(!apkt.is_null());
                let a_src_tb = sys::AVRational { num: 1, den: 44100 };
                let a_dst_tb = (**(*octx).streams.add(1)).time_base;
                let write_packet = |pkt: *mut sys::AVPacket| {
                    (*pkt).stream_index = 1;
                    sys::av_packet_rescale_ts(pkt, a_src_tb, a_dst_tb);
                    assert_eq!(sys::av_interleaved_write_frame(octx, pkt), 0);
                };
                // the flac encoder demands exactly frame_size per
                // frame (the last one may run short); the frame
                // buffer above was allocated at that block size
                let block = (*aenc).frame_size.max(1) as i64;
                let total = 44100i64;
                let mut written = 0i64;
                while written < total {
                    let n = block.min(total - written) as i32;
                    (*aframe).nb_samples = n;
                    // packed s16: both channels interleaved in plane 0
                    let plane = (*aframe).data[0] as *mut i16;
                    for s in 0..n as usize {
                        let t = (written + s as i64) as f64 / 44100.0;
                        let v = (0.5 * std::f64::consts::TAU * 440.0 * t).sin() as f32;
                        let v = (v * i16::MAX as f32) as i16;
                        *plane.add(s * 2) = v;
                        *plane.add(s * 2 + 1) = v;
                    }
                    (*aframe).pts = written;
                    assert_eq!(sys::avcodec_send_frame(aenc, aframe), 0);
                    loop {
                        let got = sys::avcodec_receive_packet(aenc, apkt);
                        if got < 0 {
                            break;
                        }
                        write_packet(apkt);
                    }
                    written += n as i64;
                }
                sys::avcodec_send_frame(aenc, std::ptr::null_mut());
                loop {
                    let got = sys::avcodec_receive_packet(aenc, apkt);
                    if got < 0 {
                        break;
                    }
                    write_packet(apkt);
                }
                sys::av_packet_free(&mut apkt);
                sys::av_frame_free(&mut aframe);
                sys::avcodec_free_context(&mut aenc);
            }

            assert_eq!(sys::av_write_trailer(octx), 0);
            sys::avio_closep(&mut (*octx).pb as *mut *mut sys::AVIOContext);
            sys::av_frame_free(&mut frame);
            sys::av_packet_free(&mut pkt);
            sys::avcodec_free_context(&mut enc);
            sys::avformat_free_context(octx);
        }
    }

    #[test]
    fn fixture_probes_and_decodes() {
        let path = Path::new("tests/fixtures/sample.mp4");
        let (duration, w, h) = probe(path).expect("probe");
        assert_eq!((w, h), (64, 64));
        assert!((duration - 1.0).abs() < 0.3, "duration {duration}");
        let render = decode_poster(path, 256).expect("poster");
        let size = render.size(0);
        assert!(u32::from(size.width) <= 256 && u32::from(size.height) <= 256);
        assert!(u32::from(size.width) > 0 && u32::from(size.height) > 0);
        // the luma bars made it through: real, non-flat content
        let bytes = render.as_bytes(0).expect("frame bytes");
        assert!(bytes.iter().any(|b| *b != 0));
        assert!(bytes.iter().any(|b| *b != bytes[0]));
    }

    #[test]
    fn preview_decodes_a_bounded_loop() {
        let path = Path::new("tests/fixtures/sample.mp4");
        let render = decode_preview(path, 256).expect("preview");
        // the fixture is one second of 25fps: the whole clip loops,
        // under every cap
        let count = render.frame_count();
        assert!((2..=25).contains(&count), "frames {count}");
        let first = render.size(0);
        let mut total = std::time::Duration::ZERO;
        for ix in 0..count {
            let size = render.size(ix);
            assert_eq!((size.width, size.height), (first.width, first.height));
            assert!(u32::from(size.width) <= 256 && u32::from(size.height) <= 256);
            // paced from the stream's own timestamps
            assert!(Duration::from(render.delay(ix)) >= Duration::from_millis(16));
            total += Duration::from(render.delay(ix));
        }
        assert!(total >= Duration::from_millis(500), "loop {total:?}");
        // real content on frame 0: the luma bars made it through
        let bytes = render.as_bytes(0).expect("frame bytes");
        assert!(bytes.iter().any(|b| *b != 0));
        assert!(bytes.iter().any(|b| *b != bytes[0]));
    }

    #[test]
    fn preview_caps_long_clips() {
        let path = Path::new("tests/fixtures/sample-long.mp4");
        let render = decode_preview(path, 256).expect("preview");
        // ten seconds of 25fps source: the frame cap lands first
        assert_eq!(render.frame_count(), 96);
        let total: Duration = (0..render.frame_count())
            .map(|ix| Duration::from(render.delay(ix)))
            .sum();
        assert!(total <= Duration::from_secs(9), "loop {total:?}");
    }

    #[test]
    fn preview_fails_soft_on_junk() {
        // a text file has no video stream; raw h264 garbage must
        // not panic either: same contract as the poster
        assert!(matches!(
            decode_preview(Path::new("Cargo.toml"), 256),
            Err(PosterFail::Empty)
        ));
        let dir = std::env::temp_dir().join(format!("kuma-video-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.h264");
        write_codec_missing_stream(&path);
        assert!(matches!(decode_preview(&path, 256), Err(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_pace_survives_degenerate_timestamps() {
        let floor = std::time::Duration::from_millis(16);
        let carried = std::time::Duration::from_millis(40);
        // the head: no previous frame to delta against
        assert_eq!(delay_from_pts(None, Some(0), floor), floor);
        // a real delta
        assert_eq!(delay_from_pts(Some(0), Some(40_000), floor), carried);
        // a sub-floor delta clamps to the pace floor
        assert_eq!(delay_from_pts(Some(0), Some(1_000), floor), floor);
        // unknown and backwards timestamps carry the last pace
        assert_eq!(delay_from_pts(Some(40_000), None, carried), carried);
        assert_eq!(delay_from_pts(Some(40_000), Some(20_000), carried), carried);
    }

    #[test]
    fn heard_clock_sits_when_the_sink_underruns() {
        // two seconds written, 150ms in the sink at the write
        let (pts, latency) = (2_000_000, 150_000u64);
        // at the write: the server is playing pts - latency
        assert_eq!(heard_micros(pts, latency, 0), 1_850_000);
        // half drained
        assert_eq!(heard_micros(pts, latency, 75_000), 1_925_000);
        // drained, or the thread stalled past the buffer: the heard
        // time sits at the write's end, never runs backwards
        assert_eq!(heard_micros(pts, latency, 150_000), 2_000_000);
        assert_eq!(heard_micros(pts, latency, 400_000), 2_000_000);
    }

    /// The test sink: records what the player wrote. Latency zero,
    /// so the heard clock is the last write's end exactly.
    struct VecSink {
        samples: std::sync::Arc<std::sync::Mutex<Vec<i16>>>,
    }

    impl AudioSink for VecSink {
        fn write(&mut self, samples: &[i16]) -> bool {
            self.samples.lock().unwrap().extend_from_slice(samples);
            true
        }
        fn latency_micros(&self) -> Option<u64> {
            Some(0)
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn player_feeds_the_sink_and_anchors_the_clock() {
        let path = Path::new("tests/fixtures/sample.mp4");
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i16>::new()));
        let player = Player::open_with_sink(
            path,
            Box::new(VecSink {
                samples: recorded.clone(),
            }),
        )
        .expect("player opens");
        let started = std::time::Instant::now();
        let mut frames = 0;
        loop {
            match player.try_recv() {
                Some(PlayerMsg::Frame { .. }) => frames += 1,
                Some(PlayerMsg::Eof) => break,
                None => {
                    // the decode thread runs at its own pace: wait
                    // for it, but never forever
                    assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "player never ended"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        // the video stream still decoded alongside
        assert!(frames >= 20, "frames {frames}");
        let written = recorded.lock().unwrap();
        // one second of 44.1kHz stereo S16, plus the encoder's
        // padding
        assert!(
            (80_000..=100_000).contains(&written.len()),
            "samples {}",
            written.len()
        );
        // the mock's latency is zero: the heard clock is the last
        // write's end, about one second into the stream
        let clock = player.clock_micros().expect("the audio clock anchored");
        assert!((900_000..=1_300_000).contains(&clock), "clock {clock}");
    }

    #[test]
    fn player_seek_flushes_and_lands_fresh() {
        let path = Path::new("tests/fixtures/sample.mp4");
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i16>::new()));
        let player = Player::open_with_sink(
            path,
            Box::new(VecSink {
                samples: recorded.clone(),
            }),
        )
        .expect("player opens");
        // the header states its duration: the seek bar's total and
        // the seek keys' clamp
        let duration = player
            .duration_secs()
            .expect("the container states a duration");
        assert!((0.9..=1.3).contains(&duration), "duration {duration}");
        // paused first: the scrub's landing frame is a paused pane's
        // business. The ring parks the thread on its send long before
        // eof on a twenty-five frame stream, so the seek below always
        // lands on a live thread.
        player.set_paused(true);
        let before = player.generation();
        player.request_seek(0.8);
        let started = std::time::Instant::now();
        let landing = loop {
            match player.try_recv() {
                Some(PlayerMsg::Frame { pts, generation, .. }) => {
                    if generation > before {
                        break pts;
                    }
                    // a pre-seek frame: the decode ran from the start
                    // and can be no further in than the ring's worth
                    assert!(pts < 0.35, "pre-seek pts {pts}");
                }
                Some(PlayerMsg::Eof) => panic!("eof before the seek's landing"),
                None => {
                    // the decode thread runs at its own pace: wait
                    // for it, but never forever
                    assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "the seek never landed"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        };
        // the landing is a keyframe at or before the target: the
        // fixture's twelve-frame gop puts one at half a second
        assert!((0.35..=0.85).contains(&landing), "landing {landing}");
        // resume while the stream is still live: the audio route
        // runs again from the seek's position, and the clock
        // re-anchors past the landing, never back at the start. The
        // ring must drain while polling or the thread parks on its
        // send and the audio stops.
        player.set_paused(false);
        let clock = loop {
            if let Some(c) = player.clock_micros()
                && c >= 350_000
            {
                break c;
            }
            loop {
                match player.try_recv() {
                    Some(PlayerMsg::Frame { .. }) => continue,
                    Some(PlayerMsg::Eof) => {
                        panic!("the stream ended before the clock re-anchored")
                    }
                    None => break,
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the clock never re-anchored"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!((350_000..=1_300_000).contains(&clock), "clock {clock}");
        // the rest of the stream decodes forward from the landing,
        // all of the new generation, out to eof
        let mut fresh: Vec<f64> = Vec::new();
        loop {
            match player.try_recv() {
                Some(PlayerMsg::Frame { pts, generation, .. }) => {
                    assert!(generation > before, "a stale frame after the landing");
                    assert!(
                        pts > fresh_last(&landing, &fresh),
                        "pts {pts} out of order"
                    );
                    fresh.push(pts);
                }
                Some(PlayerMsg::Eof) => break,
                None => {
                    assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "player never ended"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    /// The newest pts a seek test has accepted, for order asserts.
    fn fresh_last(landing: &f64, fresh: &[f64]) -> f64 {
        fresh.last().copied().unwrap_or(*landing)
    }

    #[test]
    fn player_decodes_in_order_and_ends() {
        let path = Path::new("tests/fixtures/sample.mp4");
        let player = Player::open(path).expect("player opens");
        let started = std::time::Instant::now();
        let mut frames: Vec<(f64, u32, u32)> = Vec::new();
        loop {
            match player.try_recv() {
                Some(PlayerMsg::Frame { image, pts, .. }) => {
                    let size = image.size(0);
                    frames.push((pts, u32::from(size.width), u32::from(size.height)));
                }
                Some(PlayerMsg::Eof) => break,
                None => {
                    // the decode thread runs at its own pace: wait
                    // for it, but never forever
                    assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "player never ended"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        // one second of 25fps at the fixture's native size (64x64 is
        // far under the playback cap, so the scaler never touches it)
        assert!(frames.len() >= 20, "frames {}", frames.len());
        assert_eq!(frames[0].1, 64);
        assert_eq!(frames[0].2, 64);
        assert!(frames[0].0 < 0.1, "first pts {}", frames[0].0);
        // timestamps run forward: the sync's most basic demand
        for pair in frames.windows(2) {
            assert!(
                pair[1].0 > pair[0].0,
                "pts {} then {}",
                pair[0].0,
                pair[1].0
            );
        }
        // this container has no sound server: the muted mode ran,
        // so the audio clock never anchored and the video decoded
        // anyway
        assert!(player.clock_micros().is_none());
    }

    #[test]
    fn player_fails_soft() {
        // a text file has no video stream: the open errs, no thread
        // lingers, no panic
        assert!(matches!(
            Player::open(Path::new("Cargo.toml")),
            Err(PosterFail::Empty)
        ));
    }

    #[test]
    fn hostile_input_fails_soft() {
        // a text file has no video stream; a missing file has no
        // anything. Both must degrade to Err(Empty), never panic.
        assert_eq!(probe(Path::new("Cargo.toml")), None);
        assert_eq!(probe(Path::new("tests/fixtures/none.mp4")), None);
        assert!(matches!(
            decode_poster(Path::new("Cargo.toml"), 256),
            Err(PosterFail::Empty)
        ));
        assert!(matches!(
            decode_poster(Path::new("tests/fixtures/none.mp4"), 256),
            Err(PosterFail::Empty)
        ));
    }

    #[test]
    fn raw_h264_garbage_fails_soft() {
        // a raw h264 elementary stream of garbage NALs: whatever
        // this system's libav makes of it (the free build ships an
        // h264 decoder, so the decode is attempted and lands Empty;
        // a build without the decoder would answer CodecMissing),
        // the contract is no panic and never Ok
        let dir = std::env::temp_dir().join(format!("kuma-video-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.h264");
        write_codec_missing_stream(&path);
        assert!(matches!(decode_poster(&path, 256), Err(_)));
        assert!(matches!(poster_dynamic(&path, 256), Err(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decoder_availability_pins_the_free_build() {
        // mpeg4 has a decoder in every libav build; no codec id has
        // a decoder for NONE. The free build ships the common video
        // decoders (h264 included: Fedora strips encoders), so the
        // named CodecMissing state has no end-to-end trigger in
        // this container: the helper is tested directly and the
        // panes by state injection
        assert!(!super::imp::decoder_missing(sys::AVCodecID::AV_CODEC_ID_MPEG4));
        assert!(super::imp::decoder_missing(sys::AVCodecID::AV_CODEC_ID_NONE));
    }

    #[test]
    fn duration_labels() {
        assert_eq!(duration_label(3.42), Some("0:03".to_string()));
        assert_eq!(duration_label(59.9), Some("1:00".to_string()));
        assert_eq!(duration_label(3621.4), Some("1:00:21".to_string()));
        assert_eq!(duration_label(-5.0), None);
        assert_eq!(duration_label(f64::NAN), None);
    }
}
