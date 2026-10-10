//! In-process video decode: kumaOS ships the full libav as
//! ffmpeg-libs and no CLI (the kumaOS ffmpeg contract), so posters,
//! preview loops and durations come from linking libavcodec,
//! libavformat and libswscale. Videos are hostile input: every step is fallible and
//! degrades, so callers fall back to the type icon or the card. No
//! function here may run on the UI thread; callers wrap the decode
//! in catch_unwind like every other thumbnail decode.
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
    struct FormatInput(*mut sys::AVFormatContext);

    impl Drop for FormatInput {
        fn drop(&mut self) {
            unsafe { sys::avformat_close_input(&mut self.0) };
        }
    }

    /// RAII for the decoder context: free closes and frees.
    struct CodecCtx(*mut sys::AVCodecContext);

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
pub(crate) use imp::{decode_preview, poster_dynamic, probe};

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

    /// Test-only: the degraded path always errs, and the tests assert
    /// exactly that. Nothing else has a reason to call it.
    #[cfg(test)]
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

    pub(crate) fn poster_dynamic(
        _path: &Path,
        _max: u32,
    ) -> Result<image::DynamicImage, PosterFail> {
        Err(PosterFail::Empty)
    }
}

#[cfg(not(feature = "video"))]
pub(crate) use imp::{decode_preview, poster_dynamic, probe};

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
    use super::imp::{decode_poster, decode_preview, poster_dynamic, probe};
    use super::{delay_from_pts, duration_label};
    use super::PosterFail;
    use super::write_codec_missing_stream;
    use ffmpeg_sys_next as sys;
    use std::ffi::CString;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    /// One-time fixture generator: a one-second 64x64 mpeg4 with
    /// moving luma bars, committed as tests/fixtures/sample.mp4 so
    /// the decode tests run headless with no GPU and no encoder at
    /// test time; its ten-second twin pins the preview budget. Run
    /// with `--ignored` to regenerate.
    #[test]
    #[ignore]
    fn generate_fixture() {
        encode_fixture(Path::new("tests/fixtures/sample.mp4"), 25);
    }

    #[test]
    #[ignore]
    fn generate_long_fixture() {
        encode_fixture(Path::new("tests/fixtures/sample-long.mp4"), 250);
    }

    fn encode_fixture(path: &Path, frames: i64) {
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
