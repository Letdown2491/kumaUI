//! The video pane: the state machine, the present tick, the
//! controls, and the transport strip, shared by Quick Look's pane in
//! the manager and the standalone viewer's pane. The host owns where
//! the pane sits, what its close paths are, and how a fresh player
//! is asked for (the replay and the seek-at-end rebuild one); the
//! sync algorithm itself (the due threshold, the generation floor,
//! the scrub's landing) has one home here.
//!
//! Files with only an audio stream (mp3, flac, ogg audio) do not
//! reach the pane: there are no frames to present, and the pane has
//! no audio-only still yet. Audio playback, and the metadata the
//! details pane could show for audio files, are tracked in #70.
//!
//! The module compiles against the engine's portability stub too:
//! without the `video` feature `Player::open` always errs and every
//! pane stands in its card.

use crate::video::{duration_label, Player, PlayerMsg, PosterFail};
use gpui::{
    canvas, div, prelude::*, px, relative, Bounds, Context, Div, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, RenderImage, Stateful, Styled,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The pane's state: the player, the frame on screen, the clocks,
/// the scrub, and the seek bar's captured track bounds. The fields
/// are open to the hosts (labels and tests read them); the rules
/// that keep them honest live in this module's functions.
pub(crate) struct VideoPane {
    pub(crate) path: PathBuf,
    pub(crate) player: Player,
    /// The frame on screen: its pixels and its media time.
    pub(crate) frame: Option<(Arc<RenderImage>, f64)>,
    /// One frame decoded ahead of the clock, not yet due: the ring
    /// cannot be peeked, so the first not-due frame is held here,
    /// with the generation it decoded under.
    pub(crate) ahead: Option<(Arc<RenderImage>, f64, u32)>,
    /// The stream said eof and the ring is drained.
    pub(crate) eof: bool,
    /// Media time the clock stood at on the last pause (or open).
    pub(crate) clock_base: f64,
    /// Wall anchor for the current run: media time is clock_base
    /// plus wall time since this Instant.
    pub(crate) wall: std::time::Instant,
    /// The media time shown while not playing: the pause stamp, a
    /// scrub's landing, or the last frame at the end. The audio
    /// clock is stale after a scrub (no sound ran to re-anchor it),
    /// so at rest the pane shows this instead.
    pub(crate) frozen_at: f64,
    /// A scrub in flight: the target the demuxer is jumping to.
    /// The pump drops frames short of the seek's generation until
    /// one lands, then this clears.
    pub(crate) seeking: Option<f64>,
    /// The generation the last seek started from: frames stamped at
    /// or before it are stale and dropped unheard.
    pub(crate) generation: Option<u32>,
    /// The container's duration in seconds (the bar's total, the
    /// keys' clamp); 0.0 when the header stated none.
    pub(crate) duration: f64,
    /// Integer gain, 256 = unity, as the volume keys set it.
    pub(crate) volume: u32,
    /// The mute toggle: gain zero at the sink, `volume` kept for
    /// the way back.
    pub(crate) muted: bool,
    /// An armed seek-bar drag: the track's bounds (for pointer-to-
    /// fraction), the last throttled seek's instant, and the
    /// pointer's latest fraction for the release's final seek.
    pub(crate) scrub: Option<(Bounds<Pixels>, std::time::Instant, f64)>,
    /// The seek bar track's bounds, captured at paint by the
    /// track's canvas; the mouse handlers read them to map the
    /// pointer.
    pub(crate) track: std::rc::Rc<std::cell::RefCell<Option<Bounds<Pixels>>>>,
    pub(crate) playing: bool,
    pub(crate) ended: bool,
}

impl VideoPane {
    /// The landing's build: a fresh pane from an opened player. A
    /// seek target (the ended rest's replay-into-seek) arms the
    /// scrub before the first packet and freezes the shown stamp
    /// there.
    pub(crate) fn new(path: PathBuf, player: Player, target: Option<f64>, playing: bool) -> Self {
        let duration = player.duration_secs().unwrap_or(0.0);
        let mut pane = Self {
            path,
            player,
            frame: None,
            ahead: None,
            eof: false,
            clock_base: 0.0,
            wall: std::time::Instant::now(),
            frozen_at: 0.0,
            seeking: None,
            generation: None,
            duration,
            volume: 256,
            muted: false,
            scrub: None,
            track: std::rc::Rc::new(std::cell::RefCell::new(None)),
            playing,
            ended: false,
        };
        if let Some(target) = target {
            pane.seeking = Some(target);
            pane.generation = Some(pane.player.generation());
            pane.frozen_at = target;
            pane.clock_base = target;
            pane.player.request_seek(target);
        }
        pane
    }

    /// Media time now: the audio clock when the player has sound
    /// (absolute media time, as heard through the sink), the wall
    /// clock plus the pause stamp when muted. The audio clock
    /// freezes itself while parked: writes stop, the interpolation
    /// runs out of the sink's latency, the clock sits at the last
    /// written sample, which is where the resume continues. Muted
    /// paused time does not advance: the pump only reads this while
    /// playing, and a pause stamps clock_base first. At rest the
    /// frozen stamp wins over both: after a scrub the audio clock
    /// is stale until the sound runs again.
    pub(crate) fn media_now(&self) -> f64 {
        if !self.playing {
            return self.frozen_at;
        }
        match self.player.clock_micros() {
            Some(micros) => micros as f64 / 1_000_000.0,
            None => self.clock_base + self.wall.elapsed().as_secs_f64(),
        }
    }

    /// The gain the sink should run at: zero while muted.
    pub(crate) fn gain(&self) -> u32 {
        if self.muted {
            0
        } else {
            self.volume
        }
    }

    /// The position the time label and the bar show: a scrub shows
    /// its target while in flight, the clock runs otherwise.
    pub(crate) fn shown_at(&self) -> f64 {
        self.seeking.unwrap_or_else(|| self.media_now())
    }
}

/// What a host provides: where the pane lives, and how its close
/// and rebuild paths run. `stop_player` is the teardown (the held
/// tile drops, the Player's drop wakes the decode thread, the ask
/// flag resets); `replay` is a fresh request on a new player (the
/// ended rest's space, a seek at the end), after the caller's stop.
pub(crate) trait Host: 'static + Sized {
    fn pane(&mut self) -> &mut Option<VideoPane>;
    /// The open's one-ask flag: one ask per open path, reset by the
    /// host's stop so a reopen asks fresh.
    fn open_asked(&mut self) -> &mut bool;
    /// The stale-landing check: is this path still the file the
    /// host wants? A host that moved on drops the landing unheard.
    fn pane_wants(&self, path: &Path) -> bool;
    /// Where a failed open lands (the named card's state).
    fn pane_failed(&mut self, path: &Path, fail: PosterFail);
    fn stop_player(&mut self, cx: &mut Context<Self>);
    fn replay(
        &mut self,
        path: PathBuf,
        target: Option<f64>,
        playing: bool,
        cx: &mut Context<Self>,
    );
}

/// The pane's open: the engine's open runs on the background
/// executor (libav never on the UI thread), and the landing drops
/// quietly when the host moved on to another file while it ran.
pub(crate) fn request_player<H: Host>(
    path: PathBuf,
    target: Option<f64>,
    playing: bool,
    h: &mut H,
    cx: &mut Context<H>,
) {
    if *h.open_asked() {
        return;
    }
    *h.open_asked() = true;
    cx.spawn(async move |this, cx| {
        let bg_path = path.clone();
        let opened = cx
            .background_spawn(async move {
                std::panic::catch_unwind(|| Player::open(&bg_path))
                    .unwrap_or(Err(PosterFail::Empty))
            })
            .await;
        let update = this.update(cx, |this, cx| {
            // the host may have moved to another file while the open
            // ran: a stale landing drops unheard, and the ask flag
            // stays spent (the next stop resets it)
            if !this.pane_wants(&path) {
                return;
            }
            match opened {
                Ok(player) => {
                    *this.pane() = Some(VideoPane::new(path, player, target, playing));
                    cx.notify();
                }
                Err(fail) => {
                    this.pane_failed(&path, fail);
                    cx.notify();
                }
            }
        });
        if let Err(err) = update {
            log::error!("video player open failed: {err:#}");
        }
    })
    .detach();
}

/// The present tick, run once per compositor frame while a video
/// plays: drain the frames the clock has reached (the newest due
/// wins; frames it displaces are skipped without display, the
/// late-drop rule), hold the first not-yet-due frame back, drop the
/// displaced tile (ADR-0016), and park the loop at the stream's
/// end. The threshold is a frame's own display duration when the
/// frame after it is decoded, clamped to the sync band; the clock
/// is the audio sink's, wall time when muted.
pub(crate) fn pump<H: Host>(h: &mut H, cx: &mut Context<H>) {
    let ended = h.pane().as_ref().is_some_and(|vid| vid.ended);
    if ended {
        return;
    }
    // a scrub pumps while paused too: its landing frame must
    // reach the screen without a resume
    let (playing, scrubbing) = h
        .pane()
        .as_ref()
        .map_or((false, false), |vid| (vid.playing, vid.seeking.is_some()));
    if !playing && !scrubbing {
        return;
    }
    let clock = h.pane().as_ref().unwrap().media_now();
    // the present slack: a frame's own display duration (the pts
    // delta to the frame after it) when that one is decoded,
    // clamped to the sync band; the floor alone when it is not
    let threshold = {
        let pane = h.pane().as_ref().unwrap();
        match (&pane.ahead, &pane.frame) {
            (Some((_, ahead_pts, _)), Some((_, pts))) => {
                (ahead_pts - pts).clamp(0.04, 0.1)
            }
            _ => 0.04,
        }
    };
    let mut pending = h.pane().as_mut().unwrap().ahead.take();
    let mut due: Option<(Arc<RenderImage>, f64)> = None;
    loop {
        let next = match pending.take() {
            // an ahead frame passed the filter on an earlier
            // pump: its stored generation is its own
            Some((image, pts, generation)) => Some((image, pts, generation)),
            None => match h.pane().as_mut().unwrap().player.try_recv() {
                Some(PlayerMsg::Frame {
                    image,
                    pts,
                    generation,
                }) => Some((image, pts, generation)),
                Some(PlayerMsg::Eof) => {
                    h.pane().as_mut().unwrap().eof = true;
                    None
                }
                None => None,
            },
        };
        let Some((image, pts, generation)) = next else {
            break;
        };
        let pane = h.pane().as_mut().unwrap();
        // a seek's stale frames drop unheard: anything stamped
        // at or before the seek's generation decoded before the
        // jump. Never seeked means no floor yet (generation is
        // None).
        if pane.generation.is_some_and(|g| generation <= g) {
            continue;
        }
        if pane.seeking.is_some() {
            // the scrub's landing: the first frame of the new
            // generation shows the target and ends the scrub.
            // The clock test does not apply here (the audio
            // clock is stale until the sound runs again); the
            // landing frame's own pts is the position shown.
            pane.seeking = None;
            pane.frozen_at = pts;
            due = Some((image, pts));
            break;
        }
        if pts <= clock + threshold {
            due = Some((image, pts));
        } else {
            pending = Some((image, pts, generation));
            break;
        }
    }
    h.pane().as_mut().unwrap().ahead = pending;
    if let Some((image, pts)) = due {
        let pane = h.pane().as_mut().unwrap();
        if let Some((old, _)) = pane.frame.replace((image, pts)) {
            cx.drop_image(old, None);
        }
    }
    let pane = h.pane().as_mut().unwrap();
    if pane.eof && pane.ahead.is_none() {
        // the stream ran out and everything due is on screen:
        // hold the last frame, park the loop (one notify for the
        // state change)
        pane.playing = false;
        pane.ended = true;
        pane.frozen_at = pane.frame.as_ref().map_or(pane.frozen_at, |(_, pts)| *pts);
        pane.seeking = None;
        cx.notify();
    }
}

/// Space while a video is up: pause, resume, or (at the end)
/// replay from the head. Returns false when no player owns the
/// key, so the caller's own space meaning can have it.
pub(crate) fn toggle<H: Host>(h: &mut H, cx: &mut Context<H>) -> bool {
    let state = h
        .pane()
        .as_ref()
        .map(|vid| (vid.ended, vid.playing, vid.path.clone()));
    let Some((ended, playing, path)) = state else {
        return false;
    };
    if ended {
        // replay: a fresh player and the clock back to zero; the
        // held tile drops with the teardown
        h.stop_player(cx);
        h.replay(path, None, true, cx);
        return true;
    }
    let pane = h.pane().as_mut().unwrap();
    if playing {
        // stamp before parking: the muted mode's clock_base is
        // where the wall clock resumes; the audio clock freezes
        // itself (and this stamp is ignored there). The stamp
        // must read the running clock, so it precedes the flip.
        pane.frozen_at = pane.media_now();
        pane.clock_base = pane.frozen_at;
        pane.playing = false;
        pane.player.set_paused(true);
    } else {
        pane.playing = true;
        pane.wall = std::time::Instant::now();
        pane.player.set_paused(false);
    }
    cx.notify();
    true
}

/// The video rung's seek, relative: j and l step ten seconds
/// from the shown position (the clock while playing, the frozen
/// stamp at rest).
pub(crate) fn seek<H: Host>(h: &mut H, delta: f64, cx: &mut Context<H>) {
    let from = h
        .pane()
        .as_ref()
        .map(|vid| vid.shown_at())
        .unwrap_or(0.0);
    seek_to(h, from + delta, cx);
}

/// The video rung's seek, absolute: the demuxer jumps, both
/// decoders and the sink flush, and the pane drops frames short
/// of the new generation until the landing arrives. At the end a
/// seek replays into the target, paused. The target stops a
/// decisecond short of the duration: a jump to the very end
/// demuxes straight into eof, and that state is the ended rest's
/// own (space replays), not a seek's.
pub(crate) fn seek_to<H: Host>(h: &mut H, target: f64, cx: &mut Context<H>) {
    let (duration, ended, path) = match h.pane().as_ref() {
        Some(vid) => (vid.duration, vid.ended, vid.path.clone()),
        None => return,
    };
    let target = if duration > 0.0 {
        target.clamp(0.0, (duration - 0.1).max(0.0))
    } else {
        target.max(0.0)
    };
    if ended {
        // the decode thread left at eof: replay into the target
        // on a fresh player, paused at the landing
        h.stop_player(cx);
        h.replay(path, Some(target), false, cx);
        return;
    }
    let pane = h.pane().as_mut().unwrap();
    pane.seeking = Some(target);
    pane.eof = false;
    pane.generation = Some(pane.player.generation());
    pane.player.request_seek(target);
    // the muted clock re-anchors at the target; the audio clock
    // re-anchors itself when the post-seek sound lands
    pane.clock_base = target;
    pane.wall = std::time::Instant::now();
    cx.notify();
}

/// The video rung's volume: integer gain steps, 256 = unity.
/// Adjusting while muted unmutes.
pub(crate) fn volume_step<H: Host>(h: &mut H, step: i32, cx: &mut Context<H>) {
    let Some(pane) = h.pane().as_mut() else {
        return;
    };
    pane.volume = (pane.volume as i32 + step).clamp(0, 256) as u32;
    pane.muted = false;
    let gain = pane.gain();
    pane.player.set_volume(gain);
    cx.notify();
}

/// The video rung's mute: gain zero, the previous gain kept for
/// the way back.
pub(crate) fn mute_toggle<H: Host>(h: &mut H, cx: &mut Context<H>) {
    let Some(pane) = h.pane().as_mut() else {
        return;
    };
    pane.muted = !pane.muted;
    let gain = pane.gain();
    pane.player.set_volume(gain);
    cx.notify();
}

/// The seek bar's press: jump to the pointer's fraction and arm
/// the drag (the move and up handlers ride the surface's root, so
/// the pointer may leave the track).
pub(crate) fn scrub_press<H: Host>(h: &mut H, event: &MouseDownEvent, cx: &mut Context<H>) {
    let arm = h.pane().as_mut().and_then(|pane| {
        let bounds = pane.track.borrow().as_ref().copied()?;
        let x: f32 = (event.position.x - bounds.left()).into();
        let width: f32 = bounds.size.width.into();
        let fraction = (x / width.max(1.)).clamp(0., 1.) as f64;
        pane.scrub = Some((bounds, std::time::Instant::now(), fraction));
        Some(fraction * pane.duration)
    });
    if let Some(target) = arm {
        seek_to(h, target, cx);
    }
}

/// The armed drag's move: re-seek at most every 150ms, keeping the
/// pointer's fraction for the release when throttled.
pub(crate) fn scrub_move<H: Host>(h: &mut H, event: &MouseMoveEvent, cx: &mut Context<H>) {
    let step = h.pane().as_mut().and_then(|pane| {
        let (bounds, last, _) = pane.scrub?;
        if event.pressed_button != Some(MouseButton::Left) {
            pane.scrub = None;
            return None;
        }
        if pane.duration <= 0.0 {
            pane.scrub = None;
            return None;
        }
        let x: f32 = (event.position.x - bounds.left()).into();
        let width: f32 = bounds.size.width.into();
        let fraction = (x / width.max(1.)).clamp(0., 1.) as f64;
        if last.elapsed() < Duration::from_millis(150) {
            // still inside the throttle window: keep the pointer's
            // fraction for the release
            if let Some(scrub) = pane.scrub.as_mut() {
                scrub.2 = fraction;
            }
            return None;
        }
        Some((fraction, fraction * pane.duration))
    });
    if let Some((fraction, target)) = step {
        seek_to(h, target, cx);
        // the throttle's stamp updates after the seek, so a seek's
        // own frame cannot shadow the throttle window
        if let Some(scrub) = h.pane().as_mut().and_then(|p| p.scrub.as_mut()) {
            scrub.1 = std::time::Instant::now();
            scrub.2 = fraction;
        }
    }
}

/// The armed drag's release: land the pointer's last fraction.
pub(crate) fn scrub_up<H: Host>(h: &mut H, cx: &mut Context<H>) {
    let (_, _, fraction) = match h.pane().as_mut().and_then(|p| p.scrub.take()) {
        Some(scrub) => scrub,
        None => return,
    };
    let duration = h.pane().as_ref().map_or(0.0, |pane| pane.duration);
    if duration > 0.0 {
        seek_to(h, fraction * duration, cx);
    }
}

/// The seek bar: a full-width track (flex_1 in the strip) with the
/// played fraction filled and a knob on its edge, both laid by
/// relative lengths against the track. A press jumps there and arms
/// the drag; the drag re-seeks at most every 150ms; the release
/// lands the pointer's final fraction. The track's bounds come from
/// a canvas capture at paint, since the handlers only see window
/// coordinates.
fn seek_bar<H: Host>(pane: &VideoPane, cx: &mut Context<H>) -> Stateful<Div> {
    let track = pane.track.clone();
    let fill = (pane.shown_at() / pane.duration).clamp(0.0, 1.0) as f32;
    div()
        .id("ql-video-seek")
        .relative()
        .flex_1()
        .h(px(16.))
        .cursor_pointer()
        .child(
            canvas(
                {
                    let track = track.clone();
                    move |bounds, _, _| {
                        *track.borrow_mut() = Some(bounds);
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .child(
            div()
                .absolute()
                .left(px(0.))
                .top(px(6.))
                .w_full()
                .h(px(4.))
                .rounded_sm()
                .bg(crate::theme::row_hover()),
        )
        .child(
            div()
                .absolute()
                .left(px(0.))
                .top(px(6.))
                .w(relative(fill))
                .h(px(4.))
                .rounded_sm()
                .bg(crate::theme::accent()),
        )
        .child(
            div()
                .absolute()
                .left(relative(fill))
                .ml(px(-6.))
                .top(px(2.))
                .size(px(12.))
                .rounded_full()
                .bg(crate::theme::accent()),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                scrub_press(this, event, cx);
            }),
        )
}

/// The transport strip under the pane, the players' convention:
/// elapsed, the full-width bar, total. It appears once the player
/// has landed with a duration. The caller filters the pane by the
/// file it is painting (the manager's overlay must not show a
/// stale pane's strip during a flip).
pub(crate) fn footer<H: Host>(pane: Option<&VideoPane>, cx: &mut Context<H>) -> Option<Div> {
    let pane = pane?;
    if pane.duration <= 0.0 {
        return None;
    }
    let now = duration_label(pane.shown_at()).unwrap_or_default();
    let total = duration_label(pane.duration).unwrap_or_default();
    Some(
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .pb_2()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(crate::theme::text_dim())
                    .child(now),
            )
            .child(seek_bar(pane, cx).flex_1())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(crate::theme::text_dim())
                    .child(total),
            ),
    )
}
