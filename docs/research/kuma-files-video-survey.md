# In-process video playback for kuma-files: audio output, sync, and presentation survey

2026-10-12

Primary-source survey written to inform kumaUI issue 56, phase 2 of the kumaOS
ffmpeg contract: Quick Look plays the full video in-process, with audio, so
Celluloid can be dropped. The governing issue parks the plan in three steps:
muted video in the pane, then audio via libav to pipewire with real A/V sync
("the hard 20 percent; never fake it with sleep loops"), then VA-API as a
stretch goal behind capability checks. This document researches the open seams
that step 2 depends on: which audio output binding a Rust gpui app can use
today, what the appliance image actually ships, how ffplay and mpv structure
A/V sync, how gpui can present decoded frames, what ffmpeg-sys-next 9 gives
the decode loop, how all of it is tested without pipewire or a GPU, and the
control surface the pane needs to beat Celluloid.

Method: every load-bearing claim is cited inline to a primary source, fetched
on 2026-10-12 unless stated otherwise: crate metadata from crates.io, API
docs from docs.rs and the upstream doc hosts, FFmpeg and PulseAudio headers
from the upstream git masters, the vendored gpui and gpui_wgpu sources in
`vendor/zed/crates/`, and the kumaOS clone at `~/Documents/kuma` (read-only;
nothing was written, committed, or pushed there). Image package facts come
from `src/containerfile/blocks.rs` in that clone, cross-checked against the
Fedora 44 RPM database on this dev host where a package's file list is the
claim. Repo file references are the tree at the time of writing.

## The governing issue, compressed

Issue 56 (Letdown2491/kumaUI#56, fetched 2026-10-12) fixes the order and the
bars:

1. Video-only, muted, in the details preview pane.
2. Audio via libav to pipewire with real A/V sync. The sync is the hard 20
   percent; never fake it with sleep loops.
3. VA-API hardware decode (libav vaapi hwaccel, dmabuf, zero-copy wgpu import)
   is a stretch goal, strictly behind a capability check with software
   fallback. Ship software decode everywhere first.

Bars: H.264 and HEVC in mp4/mkv play smoothly at 1080p (software decode
acceptable); inline playback must beat Open in Celluloid before it replaces
it; Celluloid stays the Enter handoff until then. Hard constraints inherited
from the contract: decode never on the UI thread; corrupt or truncated input
degrades to a placeholder, the app never panics; plain RGBA upload works
everywhere (the dmabuf zero-copy path must never be the only path, kumaOS#36:
wgpu on llvmpipe plus Vulkan dmabuf import is the path that segfaulted the
greeter on runners); no test may require a GPU. Out of scope: encoding and
transcoding, subtitles, network streams, playlists.

Issue 25 (the Koguma charter, fetched 2026-10-12) adds a portability value
that shapes the audio choice below: "View layers travel; domain services stay
home", "The only kumaOS-specific things are defaults, not dependencies", and
the UX principles (discoverable shortcuts, honest empty states, rename in
place) that bound the control surface later in this document.

## What the appliance image actually ships

All paths below are in the kumaOS clone, `src/containerfile/blocks.rs`.

- The niri desktop set ships the PipeWire stack explicitly:
  `pipewire`, `pipewire-pulseaudio`, `wireplumber` (lines 26 to 28). The
  comment above the cosmic list explains why pipewire is named at all:
  nothing in the session requires the daemon, only its client library, so the
  packages are declared rather than assumed (lines 188 to 191). The same
  three ride the cosmic set (lines 204 to 206).
- `pipewire-pulseaudio` is Fedora's package for the PulseAudio replacement
  server: on this dev host (Fedora 44, pipewire-pulseaudio 1.6.9-1.fc44) it
  contains `/usr/bin/pipewire-pulse`, the `pipewire-pulse.service` and
  `.socket` user units, and `libpipewire-module-protocol-pulse.so`. So the
  appliance runs a PulseAudio-native server on the PipeWire graph. The
  pipewire-pulse(1) man page (docs.pipewire.org, PipeWire 1.6.9) says the
  daemon "is a drop-in replacement for the PulseAudio daemon" and creates the
  native protocol pulseaudio socket under `XDG_RUNTIME_DIR`
  (https://docs.pipewire.org/page_man_pipewire-pulse_1.html).
- `pulseaudio-libs` (the libpulse client library) is NOT declared anywhere in
  blocks.rs. It arrives by dependency: the niri set declares `pavucontrol`
  (line 88), and pavucontrol 6.2 on this dev host carries the soname
  requirements `libpulse.so.0()(64bit)` and `libpulse-mainloop-glib.so.0`
  (`rpm -q --requires pavucontrol`); on Fedora those are provided by
  `pulseaudio-libs` 17.0, which also ships `libpulse-simple.so.0`, the
  library that defines `pa_simple_new` (verified: `rpm -qf
  /usr/lib64/libpulse-simple.so.0` answers pulseaudio-libs-17.0-9.fc44, and
  `nm -D` shows `pa_simple_new@@PULSE_0` defined in it). Caveat stated
  honestly: this chain is measured on the dev host, not inside a composed
  kumaOS image; before code lands, `rpm -q pulseaudio-libs` in the composed
  image should confirm it.
- `pipewire-alsa` is NOT shipped (no `alsa` hit in any package list beyond
  `alsa-*` absence; the only ALSA-adjacent strings in blocks.rs are
  comments). What that means for cpal is in the next section. On this dev
  host, where pipewire-alsa 1.6.9 happens to be installed, the package ships
  `/usr/share/alsa/alsa.conf.d/99-pipewire-default.conf`, whose entire job is
  to redefine ALSA's `pcm.!default` as `type pipewire` (file read on this
  host, 2026-10-12). Without that file, an ALSA client opening `default`
  talks to the hardware device directly and is not a PipeWire client.
- libav: the image swaps Fedora's stripped libs for rpmfusion's full build
  under the same sonames. The comment on `ffmpeg_libs()` (lines 370 to 376)
  says the swap exists so Koguma "decodes video in-process through libav, for
  the previews and the player" (the image side already anticipates this work),
  names the mechanism (`dnf install --allowerasing ffmpeg-libs` at line 378),
  and states Fedora and rpmfusion are on the same ffmpeg release, 8.1. The
  freeworld VA-API driver block above it (lines 360 to 371) restores hardware
  decode with a version-lag guard that traces back to the greeter segfault
  (kumaOS#36). On this dev host, ffmpeg-libs 8.1.3-1.fc44 (the rpmfusion
  build) contains `libavcodec.so.62`, `libavformat.so.62`,
  `libswresample.so.6` (`rpm -ql ffmpeg-libs`): libswresample is present, so
  the audio resample path needs no image change.
- Celluloid is the shipped player, as a Flathub app in the declarations
  (`examples/niri.toml` line 81, also cosmic.toml line 43 and
  `examples/vm.kuma.toml` line 36), and MIMEAPPS pins `video/mp4`,
  `video/webm`, `video/ogg`, `video/x-matroska`, `audio/mpeg`, `audio/flac`,
  `audio/ogg`, and `audio/webm` to `io.github.celluloid_player.Celluloid.desktop`
  (blocks.rs lines 2562 to 2569).
- Nothing else in the image plays audio in-process that Koguma could reuse:
  no mpv or libmpv, no gstreamer, no alsa-utils anywhere in blocks.rs. The
  shell's own audio story is `wpctl` spawns (kuma-shell/src/sysmon.rs, and
  ADR-0006's seam), not a client library. Video playback would be the first
  native audio client in the workspace.
- The kumaUI build container (containers/kuma-dev-rust.containerfile lines 2
  to 4) installs `libavcodec-free-devel libavformat-free-devel
  libavutil-free-devel libswscale-free-devel clang-devel`. Audio work adds
  build-side devel packages here (exactly which, per candidate, below); the
  runtime image needs none of them beyond what it already ships.

## Audio output binding: the three candidates

The seam a video player needs from an audio output is narrower than any
general audio API: write interleaved samples of a fixed format, block or
backpressure when the server is full, read back the latency (how many
milliseconds sit between "written" and "audible"), flush on seek, and fail
cleanly when no server runs. The three candidates are compared on that seam,
on threading, and on maintenance health.

### Candidate 1: the official `pipewire` crate (pipewire-rs)

Version and health: 0.10.1, published 2026-08-19, from
gitlab.freedesktop.org/pipewire/pipewire-rs, published by Dimitris
Papaioannou (a PipeWire contributor), MSRV 1.80, 2.59M total downloads, with
0.10.0 in May 2026 and 0.9.2 in September 2025: alive, close to upstream,
slow but steady (crates.io, fetched 2026-10-12,
https://crates.io/crates/pipewire). Unlike ffmpeg-next (a third-party safe
wrapper that could not track FFmpeg 8), this binding is hosted in the
PipeWire project's own namespace on the same GitLab as the C library, which
is a materially better maintenance posture. Its feature flags pin ABI
versions up to `v1_2_0`; the image's pipewire is 1.6.9 with the stable
`libpipewire-0.3.so.0` ABI (measured on the dev host), so linking is not in
question.

Threading model: the crate docs are blunt: "The pipewire library is not
really thread-safe, so pipewire objects do not implement Send or Sync"
(https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire/index.html,
0.10.1, fetched 2026-10-12). A `MainLoop` runs on one thread; the docs
recommend spawning it in another thread and using the built-in
`pipewire::channel` for bidirectional communication. There is also a
`thread_loop` module whose `ThreadLoop` "can be used to spawn a new thread
that runs the wrapped loop" with a lock guard for cross-thread access
(https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire/thread_loop/index.html).
A stream (`pipewire::stream::StreamRc`) is `!Send` and `!Sync`
(its doc page, fetched 2026-10-12), so the stream and its listener live on
the mainloop thread.

The data path: register a listener with a `process` callback
(`add_local_listener(...).process(...).register()`). The callback's doc:
"This is normally called from the mainloop but can also be called directly
from the realtime data thread if the user is prepared to deal with this"
(https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire/stream/struct.ListenerLocalBuilder.html,
fetched 2026-10-12). Inside the callback you `dequeue_buffer()`, fill it from
your own buffer, and drop it back (the doc example shows exactly this). So
the implementable shape is: the pw mainloop in its own thread, the decoder
thread feeding an SPSC ring buffer, the process callback draining the ring
into pw buffers. The ring buffer is the app's job (the crate ships none;
`ringbuf` or a hand-rolled SPSC, an open decision, below).

Latency readback for sync: `Stream::time()` returns a `Time` where "The time
reported by this function is updated every graph cycle, usually from the
process callback. Values such as `ticks` and `delay` are meaningful when used
together with `now`: use `pw_stream_get_nsec()` to get the current timestamp
and interpolate. With the `v0_3_50` feature enabled, this uses
`pw_stream_get_time_n()` which also populates the `buffered`, `queued_buffers`,
and `avail_buffers` fields. This function is RT-safe."
(https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire/stream/struct.StreamRc.html,
fetched 2026-10-12). That is precisely the ingredient ffplay computes by hand
(samples written minus buffered bytes, below): the graph hands you the
consumed-sample clock and the delay to the DAC.

Failure mode with no server: `Context::connect` returns
`Result<CoreBox, Error>`
(https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire/context/struct.Context.html,
fetched 2026-10-12); with no pipewire daemon there is no socket to connect to
and the call errs, so the pane degrades to the muted poster. In the test
container no pipewire runs, so the real sink is unreachable by construction
and every sink behavior must sit behind a trait (see Testing shape).

Costs: the dev container needs `pipewire-devel`; the vendoring review covers
`pipewire-sys` and `libspa`; SPA pod format negotiation is the most verbose
API of the three candidates; and the !Send model forces a dedicated thread
with lock discipline. None of this is disqualifying, it is just the most code.

### Candidate 2: libpulse simple API (`pa_simple`)

The API (src/pulse/simple.h, PulseAudio master, fetched 2026-10-12,
https://raw.githubusercontent.com/PulseAudio/pulseaudio/master/src/pulse/simple.h):
"a simple but limited synchronous playback and recording API". `pa_simple_new`
connects (NULL server means the default, which is the pipewire-pulse socket on
the appliance); `pa_simple_write` "operations always block" (the header's
transfer section); `pa_simple_get_latency` returns "the total latency of the
playback or record pipeline"; `pa_simple_flush` "will throw away all data
currently in buffers"; `pa_simple_drain` waits until sent data has played.
One stream per connection. Six functions total; the ABI has been frozen since
2004 (the header's own copyright).

Compatibility fact: on kumaOS the pulse server is `pipewire-pulse` (shipped,
see the image section), whose man page calls it "a drop-in replacement for
the PulseAudio daemon": a pa_simple client is a first-class client of the
PipeWire graph, visible to pavucontrol and wpctl as a stream with volume and
routing like any other. On non-kumaOS distros, libpulse-simple exists on
every desktop that ships either PulseAudio or PipeWire (the latter ships it
as the compat client library), so this is the most portable of the three
bindings, which matters because Koguma is portable by construction (issue 25).

Threading model: none imposed. The natural shape for a player: one writer
thread; the decoder resamples audio into S16NE and calls `pa_simple_write`,
whose blocking is the backpressure (when the server buffer is full, the
decoder thread parks in the write, which is exactly the pacing a decode loop
needs). Latency readback is a server round trip per call, on the writer
thread between writes; at 24 to 60 calls per second this is microseconds on a
unix socket. The sync math (below) is ffplay's: audible position = samples
written over rate, minus the latency the server reports.

Failure mode with no server: `pa_simple_new` returns NULL and the error
parameter carries the reason; the error enum defines `PA_ERR_CONNECTIONREFUSED`
(src/pulse/def.h line 478, "Connection refused",
https://raw.githubusercontent.com/PulseAudio/pulseaudio/master/src/pulse/def.h,
fetched 2026-10-12). In the test container (no pipewire, no pipewire-pulse)
the real sink cleanly fails to open and the pane degrades to the muted
poster.

Implementation shape in this repo: the workspace precedent is the sys crate
plus own thin RAII wrappers (kuma-files/Cargo.toml lines 34 to 42 documents
why ffmpeg-next was dropped for ffmpeg-sys-next). Two ways to own these six
functions:

- A thin hand-rolled FFI, dlopen'ing `libpulse-simple.so.0` at runtime.
  Roughly a hundred lines: six functions plus the `pa_sample_spec` struct.
  Zero new crates, zero build-container devel packages, and the portability
  rule ("libav is a capability, not a requirement", kuma-files/src/video.rs
  lines 9 to 17) extends naturally: audio is a capability detected at
  runtime; without the library the pane stays muted and honest.
- The `libpulse-simple-binding` crate (2.29.0, March 2025) over
  `libpulse-binding` (2.30.1, April 2025) over `libpulse-sys` (1.23.0), all
  maintained by one author at github.com/jnqnfe/pulse-binding-rust
  (crates.io, fetched 2026-10-12). Maintained, but a one-person effort, and
  it would still want the same trait seam in front of it.

Costs: it depends on pipewire-pulse running (it is shipped and enabled as
user units in the image; on odd non-kumaOS systems without any pulse server,
audio is simply absent, same degradation as above). `pa_simple_get_latency`
is a round trip, not a per-graph-cycle push (ffplay covers the gap by
interpolation, below). The simple API has no per-stream volume control:
volume is software gain on the samples, which is fine for Quick Look because
device volume is the session's business (ADR-0006, wpctl). No per-stream
controls beyond flush/drain.

### Candidate 3: cpal

Version and health: 0.18.2, published 2026-08-16, RustAudio/cpal, the
canonical low-level cross-platform audio I/O crate (crates.io, fetched
2026-10-12). The README (master, fetched 2026-10-12) lists Linux default
backend ALSA with optional JACK, PipeWire, and PulseAudio backends, and
notes "ALSA is needed even when using JACK, PipeWire, or PulseAudio": the
ALSA development files are a hard build dependency on Linux regardless of
the backend chosen.

Routing on the appliance: the image does not ship `pipewire-alsa` (image
section above), so an ALSA-host cpal stream would open the hardware device
directly, bypassing PipeWire and competing with it for the device (the exact
configuration pipewire-alsa's `pcm.!default` rewrite exists to prevent).
Making cpal-ALSA a native PipeWire client on kumaOS would require an image
change. cpal's optional PipeWire backend (minimum PipeWire 0.3.53 per the
README's version table) would be native, but then cpal is a thin veil over
the same pw_stream that pipewire-rs binds directly, minus the specifics.

Threading and latency: cpal is callback-based; the callback runs on the
driver's audio thread and you feed it from a ring buffer. 0.18 grew a real
clock API: `StreamTrait::buffer_size()` and `StreamTrait::now()` returning a
`StreamInstant` on the stream's monotonic clock (src/traits.rs at tag v0.18.2,
fetched 2026-10-12), and per-callback `OutputStreamTimestamp { callback,
playback }` where `playback` is "The predicted instant that data written will
be delivered to the device for playback" (src/timestamp.rs at tag v0.18.2);
the per-host time sources table lists ALSA as `snd_pcm_status_get_htstamp()`
and the PipeWire backend as `pw_stream_get_time_n()`. That is workable sync
input: playback minus callback is the buffered latency.

Failure mode with no device: in the test container there is no `/dev/snd`;
device enumeration and stream construction fail with device-not-available
class errors, again behind the trait seam.

Verdict: strictly dominated here. Its ALSA default is not a native PipeWire
client on kumaOS (no image change, no go), its PipeWire backend is less
capable than binding pw_stream directly, and it adds a mandatory ALSA build
dependency that buys nothing on the appliance. cpal is the right crate for
cross-platform audio apps; this is not that app.

### Comparison

| | pipewire-rs 0.10.1 | pa_simple (libpulse) | cpal 0.18.2 (ALSA host) |
|---|---|---|---|
| Native PipeWire client on kumaOS | Yes, native protocol | Yes, via shipped pipewire-pulse (drop-in per its man page) | No (no pipewire-alsa in image; hw device direct) |
| Latency readback | stream.time(): ticks, delay, now, buffered, updated every graph cycle, RT-safe | pa_simple_get_latency: total pipeline latency, server round trip | OutputStreamTimestamp.playback (predicted DAC instant) |
| Threading | pw objects !Send/!Sync: mainloop thread owns stream; ring buffer + channel to decoder thread | Free: blocking writes on a writer thread, backpressure built in | Callback on driver thread, ring buffer |
| Who owns the clock | The graph reports consumed samples; interpolate with now | Samples written minus reported latency (ffplay's model) | Driver-reported instants |
| Flush/seek | flush(drain) on the stream | pa_simple_flush / pa_simple_drain | Drop-and-rebuild the stream |
| No-server failure | connect() errs | NULL + PA_ERR_CONNECTIONREFUSED | device-not-available errors |
| New build deps | pipewire-devel (+ pipewire-sys, libspa vendoring) | None (dlopen) or pulseaudio-libs-devel (link) | alsa-lib-devel always; ALSA not wanted here |
| New crates | pipewire, libspa, pipewire-sys | None (dlopen) or libpulse-simple-binding | cpal |
| Maintenance | Official, freedesktop-hosted, released May and Aug 2026 | Frozen ABI, six functions, binding crate is one-maintainer but current | RustAudio, active, 0.18.2 Aug 2026 |
| Portability off kumaOS | Needs libpipewire present | Plays on any PulseAudio or PipeWire desktop | Plays anywhere with ALSA |

## A/V sync architecture (ffplay is the reference, mpv agrees)

ffplay.c (fftools/ffplay.c, FFmpeg master, fetched 2026-10-12) is the
canonical small player built on the same libav this repo links, and its
comments name the model.

- Audio is the master clock by default. The sync type enum reads
  `AV_SYNC_AUDIO_MASTER, /* default choice */` (line 235) and the default is
  set at line 397 (`av_sync_type = AV_SYNC_AUDIO_MASTER`). When there is no
  audio stream the master falls back to the external (wall) clock
  (get_master_sync_type, line 2118). mpv frames it the same way in its
  manual: `--video-sync=<audio|...>` documents the mode `audio` as "Time
  video frames to audio. This is the most robust mode, because the player
  doesn't have to assume anything about how the display behaves... This is
  the default mode." (DOCS/man/options.rst, mpv master, lines 8162 to 8202,
  fetched 2026-10-12), and its default framedrop mode `vo` "drops frames
  that are outdated at the time of display" in audio sync mode (same file,
  lines 1248 to 1273). Two independent players agree: the audio clock
  drives, video presents against it, late frames drop.
- The audio clock is samples written minus latency. Decoded audio frames get
  their pts rescaled into a time base of one over sample rate (tb =
  {1, frame->sample_rate}, line 682) and the decoder carries next_pts =
  frame->pts + frame->nb_samples (line 688). When a frame is consumed,
  `is->audio_clock = af->pts + (double) af->frame->nb_samples /
  af->frame->sample_rate` (line 3157): the time the stream will have
  reached when the last sample of that frame is consumed. Then, in the
  SDL audio callback, ffplay sets the audible clock backwards by what is
  still buffered (lines 3210 to 3212): `set_clock_at(&is->audclk,
  is->audio_clock - (double)(2 * is->audio_hw_buf_size +
  is->audio_write_buf_size) / is->audio_tgt.bytes_per_sec, serial, now)`,
  with the comment "Let's assume the audio driver that is used by SDL has
  two periods." In other words: audible_pos = samples_written / rate -
  buffered_latency, evaluated at callback time. With a server that reports
  latency (pa_simple_get_latency) or reports consumed ticks
  (pw stream.time()), the same quantity comes straight from the sink.
- The video loop presents against that clock with a threshold.
  compute_target_delay (line 2221) computes diff = vidclk - master_clock,
  sets `sync_threshold = FFMAX(AV_SYNC_THRESHOLD_MIN, FFMIN(AV_SYNC_THRESHOLD_MAX, delay))`
  with `AV_SYNC_THRESHOLD_MIN` 0.04 s (line 81) and `AV_SYNC_THRESHOLD_MAX`
  0.1 s (line 83), then corrects: if the video is behind by more than the
  threshold, shorten the frame's display time (`delay = FFMAX(0, delay + diff)`);
  if ahead by more than the threshold, stretch it (double the delay, or add
  the diff once the frame duration exceeds `AV_SYNC_FRAMEDUP_THRESHOLD`,
  0.1 s, line 85). Beyond `AV_NOSYNC_THRESHOLD` (10.0 s, line 87) no
  correction is attempted. The present loop (video_refresh, lines 2271 to
  2345) advances frame_timer, updates the video clock, and drops late
  frames: when the wall clock is past frame_timer + frame duration and
  framedrop is on (default enabled when audio is master), the frame is
  skipped and `frame_drops_late++` (line 2336). Frames that decode already
  late (pts behind the master clock before they ever display) are dropped
  at decode time, `frame_drops_early++` (line 2491). Drift inside the audio
  path itself is corrected by stretching sample counts at most
  `SAMPLE_CORRECTION_PERCENT_MAX` (10 percent, line 90) averaged over
  `AUDIO_DIFF_AVG_NB` (20) frames (synchronize_audio); a Quick Look player
  can skip this refinement and drop/repeat video instead.

Concrete sync math to implement (all pure, no sleeps beyond the frame
callback):

- `clock = samples_written / rate - latency` where latency is read from the
  sink (pa_simple_get_latency or pw stream.time), in seconds.
- A frame with pts P (seconds, from the stream time base, below) and
  duration D = next.pts - P (falling back to the frame's own duration when
  the next pts is junk, per vp_duration, line 2251) is:
  - presentable when `P <= clock + threshold` with
    `threshold = clamp(D, 0.04, 0.1)`;
  - late (drop, never show) when `P + D < clock - threshold` (ffplay's late
    drop: the wall clock is past the frame's end by more than the
    threshold);
  - early (keep the current frame, i.e. repeat) when `P > clock + threshold`.
- The loop runs per compositor frame callback (gpui side below), never in a
  sleep loop; the only clock in the system is the audio sink's consumption,
  interpolated by the wall clock between reads, exactly as ffplay's
  get_clock does (pts_drift plus elapsed time, lines 2082 to 2107).

Seeking: flush the sink (pa_simple_flush), flush the decode ring, seek the
demuxer (avformat_seek_file, already used by the poster path in
kuma-files/src/video.rs lines 285 to 296), reset samples_written and the
audio_clock base to the seek target. ffplay does the same shape
(packet queue flush and clock serial bump, lines 241 to 340 of the same
file).

## Frame presentation in gpui

The vendored gpui is at vendor/zed/crates/gpui. Line references below are
that tree, read 2026-10-12.

- Repaint-at-will exists. `Window::on_next_frame(callback)` schedules a
  closure "directly after the current frame is rendered" and explicitly wakes
  the platform frame source (window.rs lines 2605 to 2612).
  `Window::request_animation_frame()` is documented as "useful for elements
  that need to animate continuously, such as a video player" (lines 2614 to
  2629): it notifies the current view on the next frame.
  `Context::on_next_frame` wraps it with the view update (app/context.rs
  lines 287 to 297). The docs also say callers of request_animation_frame
  for decorative motion should check `cx.reduce_motion()`; a video is
  content, not decoration, but the house already honors reduce_motion for
  the preview loop (kuma-files/src/video.rs lines 236 to 240) and the pane
  should keep honoring it for the ambient loop while still presenting
  user-initiated play.
- Pacing on Wayland is the compositor's. The gpui_linux Wayland window
  requests a `wl_surface.frame` callback and parks the loop between frames;
  when presentation state requires it, "the compositor paces retries" and
  the frame callback is the wake source (gpui_linux/src/linux/wayland/
  window.rs, `complete_frame`, lines 959 to 983). So request_animation_frame
  is vblank-paced under niri, which is the honest tick for the video loop
  (and the reason there must be no sleep loop: the compositor already owns
  the cadence).
- Painting an image and freeing it: `Window::paint_image` paints a
  `RenderImage` frame as a `PolychromeSprite` whose atlas tile is
  `get_or_insert_with` keyed by `RenderImageParams { image_id, frame_index }`
  (window.rs lines 4882 to 4917): the tile is minted once per image id and
  frame index, and is filled from `data.as_bytes(frame_index)`.
  `RenderImage::new` mints a fresh monotonically increasing `ImageId` per
  instance (assets.rs lines 59 to 69), so painting a new RenderImage per
  video frame does mint a new atlas tile each time. `Window::drop_image`
  removes the sprite atlas entry for every frame index of that image
  (window.rs lines 4999 to 5011) and `App::drop_image` fans out to all
  windows (app.rs lines 2904 to 2918). ADR-0016 (docs/adr/
  0016-image-registry-owns-tile-lifetime.md) is the repo rule that names the
  discipline: nothing but an explicit drop_image ever frees a tile, so every
  owner releases when its pixels leave. kuma-files already does this in the
  Quick Look flow: the held tile is dropped the moment a new decode lands
  (browser.rs lines 4648 to 4654), on close (4210 to 4212), and on file
  flip (4266 to 4268).
- The `img` element's own multi-frame machinery is the wrong tool for full
  video, and the house already knows it: an img with a multi-frame
  RenderImage cycles frame_index by each frame's `delay()` while the window
  is active and reduce_motion is off, and calls
  `window.request_animation_frame()` to keep the loop alive (elements/img.rs
  lines 316 to 346 and 387). That is exactly how the current preview loop
  works (video.rs builds a bounded multi-frame RenderImage, browser.rs paints
  it via `img(ImageSource::Render(...)).id("ql-image")`, browser.rs lines
  7443 to 7454, and the comment there notes the element id is load-bearing:
  without persistent element state the img element has no frame clock). But
  the loop's memory cap (PREVIEW_MAX_BYTES, 64 MiB, video.rs line 231) is a
  handful of 1080p frames, so full-length playback cannot ride it; full
  video must decode per frame from the file.
- The per-frame pattern that fits the house: the pane's ql_render-style slot
  (browser.rs line 1424) holds the current frame's `Arc<RenderImage>`; each
  landed frame mints one RenderImage, the previous frame's tile is dropped
  via `cx.drop_image(old, None)`, and `cx.notify()` repaints. Cost check: a
  1080p BGRA frame is 1920 times 1080 times 4 bytes, about 8.3 MB; at 24 to
  30 fps that is 200 to 250 MB/s of tile mint plus upload and drop, which is
  ordinary texture-upload traffic (the same order as a page-sized scroll of
  the wallpaper surface ADR-0016 already budgets). Nothing in gpui bounds
  the churn; the churn is bounded by only ever holding one or two frames.
- How the decode thread hands frames over: the house pattern is
  `cx.spawn` on the foreground executor plus `cx.background_spawn` for the
  decode plus `this.update(cx, ...)` plus `cx.notify()` on landing
  (browser.rs pump_thumbs, lines 1976 to 2018; the Quick Look render job,
  lines 4550 to 4661, same shape with catch_unwind around the decode, line
  4554). For video cadence the same pattern is a foreground pump task
  awaiting frames from the decoder's channel: each received frame is one
  entity update (store frame, drop previous tile, notify), and pacing comes
  from request_animation_frame in the render path. The decoder thread never
  touches the entity; it posts (frame, pts) into a bounded channel and
  blocks when full, which is the same backpressure the thumb queue has.
- gpui_wgpu (vendor/zed/crates/gpui_wgpu), the renderer the shell and
  kuma-files dev builds use: the atlas is a set of texture pages;
  `upload_texture` stages bytes and `flush_uploads` performs
  `queue.write_texture` per pending upload (wgpu_atlas.rs lines 256 to 290).
  The color format is BGRA-native: `swizzle_upload_data` swaps R and B only
  when the page format is RGBA8Unorm and passes BGRA through unchanged
  (wgpu_atlas.rs lines 381 to 392). So a per-frame 1080p BGRA upload is a
  plain linear `write_texture` of 8.3 MB, the workhorse path the atlas
  already uses for every image; nothing video-specific is missing. The
  Vulkan-dmabuf zero-copy stretch goal from issue 56 would bypass this path,
  and kumaOS#36 is the reason it can never be the only path; software decode
  plus this upload path is the everywhere baseline.
- What kuma-files has today (the shape the recommendation lands in): the
  video module is decode-only and thread-disciplined (video.rs: no function
  may run on the UI thread, every step fallible, PosterFail names the two
  degradation states, CodecMissing and Empty, lines 19 to 29). Quick Look is
  a set of ql_* fields on the browser entity plus a decode job per file kind
  (request_quicklook_render, browser.rs lines 4510 to 4662) and the overlay
  render (quicklook_overlay, lines 7231 onward). The video arm currently
  decodes a poster or a bounded head loop and pins `video/mp4` handoff to
  Celluloid only via the kumaOS MIMEAPPS; the Enter key closes Quick Look and
  calls `open_selection`, the system opener (browser.rs lines 5178 to 5189
  region of route_key). Phase 2 replaces the poster arm with a player, not
  the pane.

## Decode loop specifics (ffmpeg-sys-next 9)

- The crate: ffmpeg-sys-next 9.0.1, published 2026-10-09, repo
  github.com/zmwangx/rust-ffmpeg-sys (crates.io, fetched 2026-10-12). The
  `swresample` feature exists and is among the crate's own defaults
  (crates.io feature list for 9.0.1 and the repo Cargo.toml, fetched
  2026-10-12); kuma-files currently enables `avcodec`, `avformat`,
  `swscale` (kuma-files/Cargo.toml line 42), so audio adds `swresample` to
  that list and links `libswresample.so.6`, which the image's ffmpeg-libs
  ships (measured, image section above). The build container adds
  `libswresample-free-devel` to kuma-dev-rust.containerfile the same way
  the others are there.
- Packet and frame API: `avcodec_send_packet` takes "a single video frame,
  or several complete audio frames", the packet stays caller-owned, and
  EAGAIN means "user must read output with avcodec_receive_frame()" first
  (libavcodec/avcodec.h, master, fetched 2026-10-12, lines 2370 to 2402);
  `avcodec_receive_frame` returns 0 on success and EAGAIN when no output is
  ready (same header, lines 2404 to 2416). video.rs already implements this
  loop (video.rs lines 325 to 380); the player loop is the same shape, fed
  continuously.
- Audio formats: decoders emit planar formats for most content, for example
  `AV_SAMPLE_FMT_FLTP` ("float, planar", libavutil/samplefmt.h line 66,
  master, fetched 2026-10-12). Interleaved output for the sink comes from
  libswresample: `swr_alloc_set_opts2` builds the context (out layout,
  S16NE or F32, rate; in layout, frame format, rate), `swr_convert` converts,
  and `swr_get_delay` reports the resampler's internal delay in a time base
  (libswresample/swresample.h, master, fetched 2026-10-12, swr_convert at
  lines 314 to 315, swr_get_delay at 468 to 474). ffplay does exactly this
  conversion into its target format (audio_decode_frame, lines 3127 to
  3160). F32LE or S16NE: S16NE matches what pulse servers accept natively
  and halves the bytes; F32 avoids clipping headroom. Open decision, below.
- Timestamps: `AVFrame.pts` is "Presentation timestamp in time_base units"
  (libavutil/frame.h lines 578 to 581) and a decoded stream's time base is
  `AVStream.time_base`, set by libavformat on decode (libavformat/avformat.h
  lines 801 to 809). video.rs already handles the frame time base versus
  stream time base dance and rescales to microseconds with av_rescale_q
  (video.rs lines 341 to 362). For audio, follow ffplay: rescale the audio
  frame pts into a time base of one over sample rate using the codec
  context's pkt_timebase (ffplay.c lines 681 to 688), so the audio clock is
  sample-accurate from there on.
- The hostile-input contract carries over unchanged: every call fallible,
  `PosterFail`-style named degradation, catch_unwind at the job boundary
  (video.rs module comment lines 1 to 17 and the existing tests, lines 810
  to 871). Corrupt audio degrades to muted video, never a panic.

## Testing shape

The container that runs the tests (./scripts/build.sh test --release
-p kuma-files, inside kuma-dev-rust) has no pipewire, no pipewire-pulse, no
/dev/snd, and no GPU; the displayless smoke runs the surface lifecycle under
a headless sway (scripts/displayless-smoke.sh, read 2026-10-12). Existing
video tests already establish the house: a committed fixture
(tests/fixtures/sample.mp4) generated by an #[ignore] test, decode
determinism asserts, hostile-input degradation, and the free build's decoder
availability pinned (video.rs lines 652 to 883).

- The seam: one `AudioSink` trait owned by the player module:
  open(config), write(samples) (backpressuring), latency() (seconds),
  flush(), close(), plus a constructor function the platform code picks.
  The real sink is the pulse-simple dlopen behind the `video`-style feature
  gate (or unconditionally attempted at runtime and None without the
  library, the cleaner portable default); tests inject a mock that counts
  samples written, answers latency with a scripted value, and records
  flushes. No test touches a real server, and the mock is what makes the
  sync math testable end to end.
- Testable headless, in three layers:
  1. Decode to frames, deterministic: extend the fixture work. The committed
     mpeg4 fixture gains an audio stream (the free build's native FLAC or
     MP2 encoder covers it; the fixture generator already encodes in-process,
     video.rs lines 664 to 755), and tests assert decoded audio frame counts,
     sample rates, pts deltas, and swr output lengths: pure libav, no GPU,
     no server.
  2. Sync math as a pure function: the present/drop/repeat classifier from
     the A/V sync section takes (clock, frame_pts, frame_duration,
     threshold) and answers Present, Drop, or Repeat. Table-driven tests
     cover ffplay's threshold corners (40 ms floor, 100 ms cap, late past
     end, early repeat, the 10 s nosync bail). No clock, no threads.
  3. The pane state machine: gpui test-support, headless, the existing
     pattern. States: Poster (no audio), Playing, Paused, Failed(CodecMiss-
     ing | Empty). Tests drive transitions with injected decodes exactly the
     way the current ql_failed tests do (browser.rs lines 12319 to 12490),
     assert the held frame's tile is dropped on each transition per
     ADR-0016, and assert key handling in the route_key Quick Look rung.
- Not testable in the container, stated honestly: actual audibility, real
  latency values, and underrun behavior on a live server. Those ride the
  owner's hardware bench (the same way the probe reads as ranges, ADR-0016's
  consequences), and the displayless smoke can at most assert the app does
  not crash when the sink fails to open (no pipewire in that container is
  the realistic no-server run).

## Control surface

What the pane needs to beat Celluloid for a Quick Look glance, per issue 56's
bar ("inline playback must beat Open in Celluloid before it replaces it")
and issue 25's UX principles (discoverable shortcuts, honest states):

- Play/pause, with the state visible (a paused overlay or dimmed frame beats
  a silent stall; issue 25's honest empty states, applied to paused).
- Seek: a bar with a draggable handle, plus keyboard stepping. Seeking must
  flush the sink and reset the sync clock (A/V sync section above).
- Volume, software gain on the samples (the simple API has no stream
  volume; device volume stays the session's, per ADR-0006). Mute toggle.
- Elapsed and total time. The total already lands today: probe reads the
  container duration and the header shows it (browser.rs lines 7293 to
  7298, request_video_duration). Elapsed is the audio clock, already owned
  by the sync math.
- Keyboard: today's Quick Look rung in route_key (browser.rs, the
  quicklook.is_some() arm) binds space and escape to close, arrows to flip
  files, shift+arrows and page keys to page, +/- zoom, 0 reset, enter to
  hand off. A video pane must not silently break those: the flip keys are
  Quick Look's spine. The conflict to resolve: space (universal
  play/pause) currently closes the pane, and left/right (universal seek)
  currently flip files. Options, cheapest first: (a) video-scoped keys on
  other letters (k play/pause, j/l seek, m mute, 0 reset) leaving the Quick
  Look spine untouched; (b) space and arrows re-meaning while a video
  plays, with the header hint line updated (the pane already prints its
  hints in the header, browser.rs lines 7347 to 7357). The recommendation
  leans (b) for space only (pause is what a person presses first on a
  playing video, and space closing a playing video reads as a bug) with
  arrows left as flip, but this is the owner's call: it changes a Quick Look
  convention that all file kinds share. Left as an open decision below.
- Where it lands: the ql_* field set gains a video player state (path,
  playhead state, sink state, current frame slot), the route_key video rung
  gains the keys above, the header row gains elapsed/total and the seek
  bar, and the existing Enter handoff (close_quicklook then open_selection)
  stays until the bar is met, exactly as issue 56 orders. Out of scope per
  the issue: encoding, subtitles, network streams, playlists.

## Recommendation

- Audio binding: **pa_simple (libpulse simple API), hand-rolled thin FFI,
  dlopen'ing libpulse-simple.so.0 at runtime, behind an AudioSink trait.**
  Reasoning: on the appliance it is a native PipeWire client (pipewire-pulse
  is shipped and its man page calls it a drop-in PulseAudio daemon
  replacement), so "audio to pipewire" is satisfied without a hard
  libpipewire dependency, which honors issue 25's portability stance (Koguma
  degrades gracefully everywhere: no pulse-compatible server, no audio,
  muted pane, no crash). The six-function ABI has been frozen since 2004, so
  dlopen with runtime detection extends the house rule that libav is a
  capability, not a requirement, to audio: zero new crates, zero new build
  deps, and the failure mode (no server) is a named, tested degradation.
  The sync ingredient it hands up (total pipeline latency) is exactly the
  one ffplay computes by hand, so the math below is unchanged. The
  alternative, the official pipewire-rs crate (0.10.1, freedesktop-hosted,
  healthy), is the right choice the day Koguma wants per-stream graph
  integration deeper than Quick Look (routing hints, pw-specific controls):
  its !Send mainloop model costs a dedicated thread and an SPSC ring, and
  its time() readback is the better primitive, but for a glance player that
  is machinery without a payoff. cpal is out: the appliance ships no
  pipewire-alsa, so its ALSA host would bypass PipeWire and fight the daemon
  for the device.
- Sync architecture, one paragraph: one decoder thread owns libav (demux,
  decode video and audio, swscale video to BGRA at pane scale, swresample
  audio to S16NE); it feeds an SPSC ring of video frames to the UI and
  streams audio into the AudioSink, whose blocking write is the only
  backpressure the audio path needs. The audible position is
  samples_written / rate minus the sink-reported latency, interpolated
  between reads on the wall clock. The UI thread, woken per compositor
  frame by request_animation_frame (vblank-paced on niri via the
  wl_surface.frame callback), presents the frame whose pts the clock has
  reached: threshold clamp(frame_duration, 0.04, 0.1) seconds; frames past
  their end by more than the threshold are dropped without display; frames
  not yet due repeat the current one. No sleep loops anywhere: the audio
  consumption is the clock, the compositor is the metronome.
- gpui presentation pattern: keep the ql_render-style single-slot handoff
  the code already uses for decodes, per frame: the pump receives (frame,
  pts) from the decoder channel, stores the frame as an Arc<RenderImage>,
  cx.drop_image(previous frame), cx.notify(), and request_animation_frame
  keeps the loop alive while playing (window.rs 2614 to 2629 names the
  video-player case explicitly). The img element's built-in multi-frame
  loop stays exactly what it is today: the bounded preview loop fallback,
  not the player. This is ADR-0016's rule applied at video rate: one tile
  minted per frame, one tile freed per frame, the atlas bounded by the
  pane.
- Slice order (issue 56's order, made concrete):
  1. Playback engine, muted: decoder thread, frame ring, present loop paced
     by request_animation_frame against a wall clock stand-in for the audio
     clock, pane state machine (Poster, Playing, Paused, Failed), corrupt
     input degrades, all headless-tested (decode determinism, state
     machine, tile drop assertions). This alone replaces the poster with
     real playback and is shippable.
  2. Audio and sync: AudioSink trait plus the dlopen pulse-simple
     implementation, swresample into S16NE, the audio-clock master loop
     from the A/V sync section, seek (flush, reset clock, demuxer seek),
     and the sync math as a pure, table-tested function with the mock sink
     covering the trait.
  3. Controls and the flip decision: seek bar, volume with software gain,
     elapsed/total in the header, the keyboard rung (with the space key
     decision made), then measure the bar against Celluloid before flipping
     Enter's handoff and the MIMEAPPS pins (a kumaOS change, owner lands
     it).
  4. Stretch, behind capability checks: VA-API decode and dmabuf import,
     never the only path (kumaOS#36), software decode stays the baseline.
- Open decisions for the owner before code:
  1. Bless the pulse-compat seam (pa_simple over the shipped
     pipewire-pulse) versus the native pipewire-rs thread. This document
     recommends pa_simple; the cost is trusting pipewire-pulse as the
     appliance's always-on audio server, which the image already commits
     to.
  2. Space in Quick Look: pause a playing video (convention change for all
     file kinds) or keep space-closes and put play/pause on k. The header
     hint line must say whichever wins.
  3. Arrow keys during video: keep file-flipping (Quick Look's spine) or
     re-mean as seek while a video plays. Recommendation: keep flip, seek
     on j/l and the bar.
  4. Sample format for the sink: S16NE (half the bytes, native pulse
     format, clamp-based volume) versus F32LE (headroom, one extra nothing
     on modern CPUs). Recommendation: S16NE with volume as integer gain,
     matching pa_sample_spec's cheapest path.
  5. Whether the player state lives as ql_vid_* fields on the browser
     entity (the house pattern, and enough for one pane) or as a small
     dedicated entity. Recommendation: ql_* fields until a second consumer
     exists, per the just-in-time extraction rule (issue 25).
  6. Verify on a composed image before landing: `rpm -q pulseaudio-libs`
     (the pavucontrol dependency chain is measured, not composed) and
     confirm pipewire-pulse is enabled in the user session, so the audio
     capability check and the degraded pane have a real trigger to point
     at.
