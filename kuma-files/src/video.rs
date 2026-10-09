//! In-process video decode: kumaOS ships the full libav as
//! ffmpeg-libs and no CLI (the kumaOS ffmpeg contract), so posters
//! and durations come from linking libavcodec, libavformat and
//! libswscale. Videos are hostile input: every step is fallible and
//! degrades to None, so callers fall back to the type icon or the
//! card. No function here may run on the UI thread; callers wrap
//! the decode in catch_unwind like every other thumbnail decode.

use std::path::Path;

use ffmpeg_next as ffmpeg;

use crate::icons;

/// The container header's facts: duration in seconds when the
/// container reports one, plus the coded dimensions. No frame is
/// decoded; everything here is container and codec header.
pub(crate) fn probe(path: &Path) -> Option<(f64, u32, u32)> {    ffmpeg::init().ok()?;
    // libav logs misdetections straight to stderr; the None return
    // is the real signal, keep the journal clean
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    let ictx = ffmpeg::format::input(path).ok()?;
    let raw = ictx.duration();
    let duration = (raw >= 0).then(|| raw as f64 / 1_000_000.0)?;
    let stream = ictx.streams().best(ffmpeg::media::Type::Video)?;
    let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .ok()?
        .decoder()
        .video()
        .ok()?;
    let (width, height) = (decoder.width(), decoder.height());
    (width > 0 && height > 0).then_some((duration, width, height))
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

/// One representative frame as a BGRA `RenderImage`, no more than
/// `max` on the long edge: ten percent in, one second as the
/// ceiling (first frames run black on plenty of recordings), with
/// a first-frame retry when the seek or the decode at that point
/// comes up empty. None when the file is unreadable, undecodable,
/// or has no video stream.
pub(crate) fn decode_poster(path: &Path, max: u32) -> Option<gpui::RenderImage> {
    ffmpeg::init().ok()?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    poster(path, max, false).or_else(|| poster(path, max, true))
}

fn poster(path: &Path, max: u32, from_head: bool) -> Option<gpui::RenderImage> {
    let mut ictx = ffmpeg::format::input(path).ok()?;
    let stream = ictx.streams().best(ffmpeg::media::Type::Video)?;
    let video_index = stream.index();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .ok()?
        .decoder()
        .video()
        .ok()?;
    if !from_head {
        // the seek's timestamp runs in the container time base,
        // microseconds
        let raw = ictx.duration();
        let tenth = (raw > 0)
            .then(|| (raw as f64 * 0.1) as i64)
            .unwrap_or(1_000_000);
        ictx.seek(tenth.min(1_000_000), ..).ok()?;
    }
    let mut decoded = ffmpeg::frame::video::Video::empty();
    let mut scaler: Option<ffmpeg::software::scaling::context::Context> = None;
    for (stream, packet) in ictx.packets() {
        if stream.index() != video_index {
            continue;
        }
        decoder.send_packet(&packet).ok()?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            if scaler.is_none() {
                let (dw, dh) = fit(&decoded, max);
                scaler = ffmpeg::software::scaling::context::Context::get(
                    decoded.format(),
                    decoded.width(),
                    decoded.height(),
                    ffmpeg::format::Pixel::RGBA,
                    dw,
                    dh,
                    ffmpeg::software::scaling::flag::Flags::BILINEAR,
                )
                .ok();
            }
            let scaler = scaler.as_mut()?;
            let mut rgba = ffmpeg::frame::video::Video::empty();
            scaler.run(&decoded, &mut rgba).ok()?;
            let (w, h) = (rgba.width() as usize, rgba.height() as usize);
            let stride = rgba.stride(0);
            let data = rgba.data(0);
            let mut buf = Vec::with_capacity(w * h * 4);
            for row in 0..h {
                buf.extend_from_slice(&data[row * stride..row * stride + w * 4]);
            }
            let image = image::RgbaImage::from_raw(w as u32, h as u32, buf)?;
            return Some(icons::decode_to_render(
                image::DynamicImage::ImageRgba8(image),
                max,
                max,
            ));
        }
    }
    None
}

/// The decoded frame fitted into a box of `max` on the long edge,
/// never upscaled: the downscale happens in the scaler so no
/// full-size RGBA intermediate exists.
fn fit(frame: &ffmpeg::frame::video::Video, max: u32) -> (u32, u32) {
    let (w, h) = (frame.width() as f64, frame.height() as f64);
    let scale = (max as f64 / w).min(max as f64 / h).min(1.0);
    let dw = ((w * scale).round() as u32).max(1);
    let dh = ((h * scale).round() as u32).max(1);
    (dw, dh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// One-time fixture generator: a one-second 64x64 mpeg4 with
    /// moving luma bars, committed as tests/fixtures/sample.mp4 so
    /// the decode tests run headless with no GPU and no encoder at
    /// test time. Run with `--ignored` to regenerate.
    #[test]
    #[ignore]
    fn generate_fixture() {
        ffmpeg::init().unwrap();
        let path = Path::new("tests/fixtures/sample.mp4");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut octx = ffmpeg::format::output(path).unwrap();
        let global_header = octx
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);
        let codec = ffmpeg::codec::encoder::find(ffmpeg::codec::Id::MPEG4);
        let mut ost = octx.add_stream(codec).unwrap();
        let mut enc = ffmpeg::codec::context::Context::new_with_codec(codec.unwrap())
            .encoder()
            .video()
            .unwrap();
        enc.set_width(64);
        enc.set_height(64);
        enc.set_format(ffmpeg::format::Pixel::YUV420P);
        enc.set_frame_rate(Some(ffmpeg::Rational::new(25, 1)));
        enc.set_time_base(ffmpeg::Rational::new(1, 25));
        if global_header {
            enc.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        }
        ost.set_parameters(&enc);
        let mut opened = enc.open().unwrap();
        ost.set_parameters(&opened);
        octx.write_header().unwrap();
        let src_tb = ffmpeg::Rational::new(1, 25);
        let dst_tb = octx.stream(0).expect("stream").time_base();
        for i in 0i64..25 {
            let mut frame =
                ffmpeg::frame::video::Video::new(ffmpeg::format::Pixel::YUV420P, 64, 64);
            frame.set_pts(Some(i));
            // moving luma bars: the poster test asserts real content
            {
                let stride = frame.stride(0);
                let y = frame.data_mut(0);
                for row in 0..64usize {
                    for col in 0..64usize {
                        y[row * stride + col] = ((row + col + i as usize * 8) % 256) as u8;
                    }
                }
            }
            for plane in 1..3 {
                let _stride = frame.stride(plane);
                let chroma = frame.data_mut(plane);
                for cell in chroma.iter_mut() {
                    *cell = 128;
                }
            }
            opened.send_frame(&frame).unwrap();
            drain(&mut opened, &mut octx, src_tb, dst_tb);
        }
        opened.send_eof().unwrap();
        drain(&mut opened, &mut octx, src_tb, dst_tb);
        octx.write_trailer().unwrap();
    }

    fn drain(
        opened: &mut ffmpeg::codec::encoder::video::Video,
        octx: &mut ffmpeg::format::context::Output,
        src_tb: ffmpeg::Rational,
        dst_tb: ffmpeg::Rational,
    ) {
            let mut packet = ffmpeg::Packet::empty();
        while opened.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.rescale_ts(src_tb, dst_tb);
            packet.write_interleaved(octx).expect("mux packet");
        }
    }

    #[test]
    fn fixture_probes_and_decodes() {
        ffmpeg::init().unwrap();
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
    fn hostile_input_fails_soft() {
        ffmpeg::init().unwrap();
        // a text file has no video stream; a missing file has no
        // anything. Both must degrade to None, never panic.
        assert_eq!(probe(Path::new("Cargo.toml")), None);
        assert_eq!(probe(Path::new("tests/fixtures/none.mp4")), None);
        assert!(decode_poster(Path::new("Cargo.toml"), 256).is_none());
        assert!(decode_poster(Path::new("tests/fixtures/none.mp4"), 256).is_none());
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
