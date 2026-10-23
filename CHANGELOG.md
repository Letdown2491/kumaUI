# Changelog

Entries land with the change they describe; the next tag takes this section as its release notes. Say what changed and what a reader has to do differently. Why it changed belongs in the commit that made it. Bullets land under Added, Changed, or Fixed; `scripts/release.sh` reads those headings to infer the bump (any Added or Removed: minor; only Changed and Fixed: patch). The version lines under a release heading name each binary's version in that release: an app whose line did not move rides unchanged. Every bullet is a single line: GitHub renders newlines inside a bullet as hard breaks, so a wrapped bullet pins its text to a narrow column everywhere it is pasted.

## Unreleased

### Added

- The nostr panel gains a Vault tab: generate or import an identity in the panel (secrets ride stdin, never a command line), export a backup (0600, never overwritten), lock and unlock with the inactivity window on show, and destroy with the dry run's cost quoted and an export offered first.
- Koguma reads kumaOS's snapshot store: a clock in the toolbar opens the folder's timeline (newest first), a banner says which moment is on show, browsing inside a snapshot is read-only, and "Restore to live folder" copies back over the live tree through the conflict dialog, so the old file steps aside undoably.
- The bar gains a VPN widget, sibling of Internet: absent when NetworkManager defines no VPN connections, dim when none is up, lit with the connection's name when one is, and the Wi-Fi panel's new VPN section connects and disconnects each defined connection (optimistic flip, rollback on a failed bring-up).
- The session mirror carries the compositor's keyboard layouts: a bar widget shows the live layout once more than one is configured (hidden otherwise, click cycles), and `kuma-shell msg layout <next|prev|n>` switches it, working even with the shell down; niri and sway both speak.

### Fixed

- kuma-greeter: a failed login shows a plain card ("wrong username or password"; a lockout and a nologin denial say so) instead of greetd's raw pam text; the technical description goes to the journal.
- kuma-files: pressing an arrow key in icon view no longer crashes the window when the listing shrank (say, mid cleanup): the horizontal step still counted a partial last row's missing columns and indexed one past the end.

## v0.6.0 (2026-10-22)

kuma-shell 0.6.0
kuma-files 0.5.0
kuma-term 0.4.0
kuma-monitor 0.3.0

### Added

- Koguma's search gains a content rung: the banner's "names only" chip flips to "names + contents", and files whose bytes carry the query stream into the same result list in batches, each row showing its folder and the first matching line. A content hit stays visible no matter its name, and hidden files follow the hidden-files toggle. The walk is bounded like the name walk and stops the moment the query moves on.
- Koguma's inspector shows directory sizes: selecting a folder measures its tree in the background (regular files only, depth-capped against mount cycles), caches the total keyed by the folder's mtime, and repaints the size row when the walk lands.
- Koguma's listing sniffs unrecognized files by magic bytes: the background listing task reads the first 4 KB of files whose extension says nothing, and the row gets an honest icon and type (an extensionless png shows as an image; a jpeg wearing a .txt name keeps its bytes).

- The terminal speaks the kitty keyboard protocol: a program can ask what keyboard modes are active (`CSI ? u`) and set, push, and pop its enhancement flags (`CSI = / > / < u`), and while it asks for disambiguation the chords arrive as distinct CSI-u escapes instead of collapsing into legacy bytes: shift+enter, ctrl+tab, ctrl+letter reach programs that bind them. Without the push, every keystroke encodes exactly as it always did.
- The terminal zooms at the keyboard: `ctrl+=` and `ctrl+-` step the font a point, `ctrl+0` returns to the configured size, and the program on screen reflows the way a window resize reflows it. The config's own `font_size` never moves, so the next launch starts there.
- The terminal takes a right-click: a small Copy / Paste menu opens at the pointer (Copy dims when the grid holds no selection), and `copy_on_select true` in the config lands every finished selection on the clipboard by itself.
- Kamuy can act on what it watches: the arrow keys walk the table (wrapping, the view following), `ctrl+k` sends the cursor row's process one SIGTERM, and pressing `ctrl+k` on the same row again arms a confirm card whose Enter sends SIGKILL, since a process that ignored the polite ask is killed on purpose or not at all. Group headers and the background summary have no single pid and are skipped. The footer says what was signaled, or why nothing was.
- Kamuy's table remembers its sort and filter across launches, in a small state file under `~/.config/kuma-monitor` written atomically (Koguma's state file shape); a restored sort applies once, to the first table there is to sort.
- The location bar completes as you type, fish-style: the folders extending what you typed suggest as dim ghost text past the caret, Right (or End, already at the end of the line) lands the suggestion into the field, and Tab cycles the matches. Filesystem only, folders only, with `~` kept in the suggestions' form; hidden folders appear only when hidden files are shown. Relative paths in the bar now also resolve against the folder the tab is looking at rather than wherever Koguma was started from.
- The bar shows when the screen is being shared. Niri reports its screencasts first-party, and the shell mirrors them into a sharing indicator beside the recording dot: an amber dot naming the target (output name or window title), a click ending the portal sessions. Sharing and recording are distinct states, not merged; both can run at once.
- Audio files (mp3, flac, ogg, opus, wav, m4a) read like videos in the details pane: the header-only probe gains an audio-only variant and lands a length row, and embedded album art shows in the preview slot when the file carries it (the icon stands in when it does not). Nothing plays and nothing is claimed: audio keeps opening in the system's player, and the panes stay video-only.
- The standalone viewer (kuma-view) renders text files handed to it directly: Quick Look's capped whole-file read and reading pane, shared, with csv rows aligned like the peek's. Binary files still land the failed card. Text stays unclaimed, so double-click and xdg-open keep opening the editor; only a direct `kuma-view notes.txt` lands the text rung.

- Paste in every shell text field: Ctrl+V inserts the clipboard into the launcher query, the Wi-Fi and lock and greeter and polkit passwords, and the settings edits (Ctrl+U kills the line). The audit's #1 polish gap: a 40-char vault password no longer means typing it by hand on the lock screen.

- `kuma-shell msg notifications` opens the notifications panel, the expected pair with the `notifications-dnd` toggle beside it (which keeps its old meaning, on the CLI and on the wire).
- Notification toasts carry their action buttons, and the body's click fires the client's default action (the spec's "default" key) instead of plain dismissal.
- A burst of notifications queues instead of clobbering: one toast on stage at a time, up to three waiting their turn (a longer burst expires the oldest waiting unread), and a client replacing its own notification still updates in place.
- The history panel stacks consecutive notifications from one app under a count header with an expand toggle, so a morning of pings from one app reads as one row plus the newest card.

### Changed

- The terminal's snapshot tracks damage per viewport line: only the lines the grid actually moved re-decode (an unchanged line hands the renderer the same row it painted last frame), so a fast-emitting stream pays one line's decode instead of the screen's, and the paint buckets its selection and search washes per row instead of filtering every span against every row.
- Koguma's file operations can be stopped: a Cancel button sits in the footer beside the progress line while an operation runs, and the stop lands between files, between directory entries, and between copy chunks, so even a 50 GiB paste answers within a beat. Everything that already landed stays and undoes; a cancelled copy takes its partial destination back out (a replaced target's original goes back over it, as on any other failure).

- Koguma sorts names naturally: digit runs compare as numbers, so file2 sits before file10. The list also gains a Type sort key and column (folder, image, text, video, audio, archive, and friends), ordering by the full mime with a natural-order tiebreak.

- The tray went signal-driven: item snapshots ride the item's own SNI update signals (and bus departures reap items as they exit) instead of a 2s property re-poll of every item, with a 60s heartbeat sweep standing behind the signals as the silent-item fallback. A pixmap whose bytes hash to the tile the last read already converted is no longer re-converted per read.
- Smaller per-render and per-pass costs from the same audit: a slider drag against a wedged PipeWire coalesces the queued audio writes into the newest value instead of replaying every intermediate one, the CPU temperature sensor is resolved once and re-probed only when it stops answering, the wallpaper view resolves its path once per settings change instead of statting per render (its per-render logs demoted to debug), and the bar borrows each section's widget list instead of cloning it per render.
- The shell's network state (online, connected device, ssid, wifi radio) rides NetworkManager's own D-Bus signals instead of spawning nmcli three times every two seconds: the watcher listens once on the service-wide signal stream and recomputes the state from properties on each event. Zero fork/execs at idle for the network read. The nmcli poll stays as the fallback seam and keeps owning the Wi-Fi panel's access-point scan.
- The sysmon poll split its one serial pass into per-subsystem passes: each reads on its own cadence, gated on its own consumers. The fast audio/brightness pass (the OSD's and the keybinds' feedback path) no longer queues behind network, Bluetooth, and media scans; the network and Bluetooth state now poll on slow clocks (10s and 5s) instead of every 2s, panel-driven scans (streams, AP list, paired devices) run only while their panel is open, and a pass with no consumer at all idles to a cheap heartbeat instead of running for nobody. The Wi-Fi AP scan no longer fires while the Bluetooth panel is open, which was riding the old shared gate by accident.
- The OSD cycles when one tick changes several channels: each changed card gets its own turn on the stage (volume leading) instead of every change but the loudest being dropped, with the waiting line capped.
- The settings panel reopens on the page a person left, instead of always leading with Quick.
- The calendar's month and weekday names follow the system locale (the bar clock's date line already did).
- Wi-Fi and Bluetooth rows press like every other row: click, not mouse-down, and the media widget shows the pointer cursor (it plays and pauses on click).

### Fixed

- The idle watcher's screen-off blanked monitors only on niri: it shelled out to `niri msg` regardless of compositor, so sway sessions (explicitly supported) went dark-never, silently. The blank now routes through the session adapter seam like the focus commands (niri's power-off action, sway's `output * power off`), and a compositor with neither socket logs the gap once instead of failing mute every timeout.

- The session mirror now backs off when its compositor socket dies instead of retrying on a fixed three-second clock forever: the retry ladder climbs to a one-minute ceiling (a permanently dead socket logs ~1,400 lines a day instead of ~28,800), and an attempt that stayed up a minute resets it, so a compositor restarting on a human timescale still reconnects in seconds. Each attempt also runs on its own named thread (its blocking socket reads no longer occupy a background-executor worker), and niri's event reader now carries the same 2 MiB line cap sway's has, so a broken peer cannot balloon the line buffer without limit.
- Selecting everything in a directory and pressing Delete could kill Koguma: the trash emptied the listing while the view's virtualized window still started rows past its new end, and the render's slice panicked the process (which reads as the window closing). The window clamps to what the listing actually holds now; a scrolled icon-grid reproduction and the plain mass-trash sequence are pinned by tests.
- Forget network and unpair device confirm before they fire: the trash icon arms (label swaps to the question, self-disarms in 5s, the kit's arm-then-confirm shape), the second click acts. The updates panel's Restart now wears the destructive styling it always should have.
- The lock screen no longer blames every PAM failure on the password: a rejection still reads "wrong password", but a machine failure (broken PAM config, expired account, missing service) shows PAM's own text, with the full chain in the journal. The password field also shows a caps-lock hint while caps lock is on.
- Esc inside an inline edit field cancels the edit only: the Wi-Fi password prompt and the weather location edit no longer close the whole panel with the cancel.
- Clearing the history now tells clients: every cleared entry emits its NotificationClosed (reason 2) instead of leaving the sender hanging on a signal that never comes.
- Invoking a notification action no longer emits NotificationClosed twice.
- The platform's plain niri exchange no longer trusts the socket forever: a generous default timeout (10s) bounds a stuck peer, and reply lines are capped at 1 MiB, so a broken niri cannot balloon the buffer. Callers that install their own tighter timeout are unchanged.
- Config and state records (`write_atomic` and everything riding it: the shell's settings and updates state, Koguma's browsing state, marks, recents, and default-app choices) are fsynced before the atomic rename and land 0600: a power cut can no longer leave a renamed record empty, and where-you-are and what-you-browse data is no longer world-readable.
- Untrusted image decodes carry strict dimension and allocation limits (16K×16K, 64 MiB): a hostile oversized PNG in any browsed directory or wallpaper folder no longer spikes a gigabyte and a half of memory per decode.
- The greeter and the polkit agent match the lock screen's password hygiene: the greeter's render never clones the secret (a masked count feeds the field) and both zeroize the password once the attempt's last consumer is done, so a failed login leaves no fresh heap copies behind.
- A corrupt or torn config.toml no longer risks permanent loss: the boot load backed by defaults now backs the unreadable file up beside itself (`config.toml.invalid-<timestamp>`) before the next settings commit writes defaults over it.
- Bar widgets the sysmon full pass feeds (internet, bluetooth, media, power profile) rendered empty forever unless a sysinfo widget was also placed: the pass's run gate checked only the sysinfo five, while the same pass was their only data source. The gate now covers every fed widget, and the recording indicator no longer depends on it at all: its /proc read rides the fast pass directly (in-process, the pgrep subprocess dropped).
- Koguma's "Open Terminal Here" found no terminal on kumaOS: the candidate ladder named `$TERMINAL`, `xdg-terminal-exec`, and eight emulator binaries, none of which the image ships. kuma-term now sits right after `$TERMINAL` in the ladder.
- Directories on gvfs-FUSE mounts (samba, sftp) refresh when files change on the remote side. The bridge delivers no inotify events for remote writes, so such a listing stayed stale until F5 or a navigation; the idle tick now polls the directory's mtime and rescans on movement. A remote append to an existing file moves no directory mtime and still wants F5.

## v0.5.2 (2026-10-21)

kuma-shell 0.5.2
kuma-files 0.4.3
kuma-term 0.3.3
kuma-monitor 0.2.3

### Changed

- Release cuts commit before they build: the stamp in every installed binary now names the commit the tag names, where v0.4.1 and v0.5.1 both shipped the parent commit's stamp and needed a manual rebuild at the tag before kumaOS could pin (version, sha). release.sh asserts all five installed siblings answer the release commit's sha and that the lock did not move after the commit, then tags.
- The dev build writes the staging siblings beside the binaries, not only the session copies: kumaOS's compose stages the three .desktop entries and three icons from beside the running kuma as the release variant (@BIN@ to /usr/bin), and build.sh now refreshes those six files on every build, so a packaging change cannot leave the image's copies silently stale (the stamp gate checks binaries only).

## v0.5.1 (2026-10-21)

kuma-shell 0.5.1
kuma-files 0.4.2
kuma-term 0.3.2
kuma-monitor 0.2.2

### Changed

- The build installs kuma-greeter next to kuma-shell: binary only, no menu entry or icon (greetd runs the image's copy at login), so `--version` and the headless rehearsal scripts answer for the build they were run against.
- The sysmon full pass (the cpu cores, loadavg, temp, disk, and battery scans) runs only while a widget that feeds it is placed on the bar; with none placed the pass is skipped instead of running for nobody. The lighter AV pass stays unconditional: the OSD reads volume, mic, brightness, and the power profile with no widget placed.
- The clock's date faces follow the system locale the way the time follows the system zone: `LC_ALL`, then `LC_TIME`, then `LANG` picks the vocabulary, and weekday and month names render in the user's language (English when none is set or known, which is what the faces printed before).
- The dock stops rescanning the applications directories after three fruitless rounds for an unchanged set of unmatched windows: one bare-terminal window used to cost a full applications-dir walk every 5 seconds for the session's whole life. A changed or cleared set resets the counter, so an actually-installed app still gets its scan.

### Fixed

- The wallpapers page no longer misreads a remote file URI as a relative path: a folder picker on a mounted or remote location answers `file://otherhost/wallpapers`, and the page stripped only the scheme, reading the rest as `otherhost/wallpapers` relative to the shell's working directory. A remote answer now points at nothing, and the folder stays unset.
- The launcher's usage record writes atomically now: a crash mid-save could tear the usage JSON, and the launcher read the wreck as "no usage" and rebuilt the ranking from nothing. Every state and config record in the shell lands through the same temp-file-plus- rename write.
- A subprocess that backgrounds a pipe-holding grandchild (`sleep 999 & echo hi`) no longer wedges the poll that ran it forever: the shared bounded read (nmcli, bluetoothctl, wpctl, playerctl, widget sources) kills the whole process group at its deadline and caps what it captures, where the old reads waited on every writer of the pipe.
- A custom widget icon naming a file the widget directory does not hold falls back to themed resolution by stem: `bell.svg` now renders the theme's bell instead of nothing (the themed index keys are file stems, and the extension used to ride along).
- The monitor's disk widgets under-reported throughput and totals by exactly 512x: /proc/diskstats counts sectors, and the parse handed the raw sector counts down as if they were bytes. The parse grows them to bytes now, so the disk view's rates and the per-device sums read like every other byte figure.
- The monitor's per-process memory assumed a 4096-byte page when it grew /proc statm's page counts to bytes: on 16 KiB and 64 KiB page kernels private memory under-reported by the page-size ratio. The page size comes from the kernel now, read once.
- Night light on a compositor without the gamma protocol parks its thread after one error line instead of logging the same error every 5 seconds.
- The night-light schedule window follows DST: the local UTC offset was sampled once per process and kept forever, so the window sat an hour off after every transition. It re-samples at least every ten minutes.
- A healthy gamma control resets its rebind backoff, so a later transient failure retries in a second instead of inheriting the minute a long-ago permanent failure had backed off to.
- A failed app launch keeps the launcher panel open: activate and the mouse click closed the panel regardless, dropping the user's query along with the error.
- Wi-Fi network names containing a literal colon render and match correctly in the network panel: `nmcli -t` escapes colons as `\:`, and the panel split on every colon, both for the saved-network list and the device status line, so `My:Net` showed as `My\:Net` and never matched its own saved connection. Both paths go through the same unescape the scan list uses.
- Desktop files with `[Desktop Action]` sections no longer vanish from the greeter's session picker: a section header fell into the key=value arm, and its missing `=` aborted the whole file. Other section headers close the entry section now, and a stray line without `=` is skipped instead of fatal.
- The themed-icon index walk survives a symlink cycle in an icon tree: a stray `ln -s .. loop` walked forever under the index lock and froze the shell at the next icon resolve. Directories are canonicalized before they stack, so honest symlinked theme directories still walk and cycles stop.
- A slow wallpaper decode can no longer clobber a newer wallpaper choice: the decode adopted whatever path was current when it finished, so a quick A then B then A swap could leave A's pixels pinned under B, and a decode that lost the race pinned its tiles in the atlas unused. Adoption is gated on the path still being the one wanted; a losing decode releases its tiles.
- The weather widget fetches at startup instead of one poll interval after it: a cached location showed an empty widget for up to a quarter hour after every boot.
- Notification urgency reads the spec's wire form now: a byte in the hints (0 low, 1 normal, 2 critical). Only the legacy string spelling was understood, so every spec-compliant critical notification rendered as a normal one.
- A toast replaced by a newer notification emits the Closed signal (the expiry shape) for the notification it replaced: the client that sent it used to wait forever on a signal that never came.
- Reloading config from the settings page no longer replaces the running settings with defaults when the file fails to parse (a hand edit with a typo), and no longer writes those defaults over the file: the reload keeps the running settings and says so in the log, because destroying the config at the one moment it is being edited is the worst possible answer to a typo.
- Koguma's rename no longer silently overwrites an existing file: rename(2) replaces its destination, so committing a name that already exists destroyed that file with no dialog and no undo. The rename now refuses, keeps the editor open, and says so.
- A Koguma replace-copy no longer destroys the destination when the copy fails midway: `fs::copy` truncates the target before the first byte moves, so a full disk or an unplugged drive used to leave a good file replaced by a partial one whose undo could only delete the wreckage. The old target now steps aside first (the move's shape) and the undo puts it back instead of deleting the copy.
- A Koguma replace-copy aimed at a directory is an honest error instead of a silent merge: the conflict dialog only offers Replace for file-onto-file, so a directory landing between plan and run used to overwrite same-named children with no prompt.
- A Koguma cut survives a refused or skipped paste: the clipboard was cleared before the queue even accepted the plan, so a paste refused because another operation was running (or a conflict resolved as Skip for all) destroyed the cut and nothing moved. The clipboard now dies only when the queue accepts the plan.
- The session mirror no longer logs an error, repeating on every reconnect timer, for a session event stream that ended cleanly: a clean end is a warning about the retry, not an error.
- The greeter recognized a version flag anywhere in its arguments: a headless rehearsal command spelling `-V` in a later position printed the version line and exited instead of logging in. It is a first-argument flag now, like every binary in the family.
- Long widget text (a media title, a custom widget's script line) ellipsizes inside its section instead of shoving the neighbors off the bar, and all three bar sections clip overflow at their edge the way the center section always did.
- The battery widget clamps its label and tooltip to 100% the way its icon already clamped, so a nonstandard driver reporting above-full cannot render a nonsense "150%".
- The brightness tooltip reads "Brightness unknown" when its reading is missing, the way every other widget's unknown state reads, instead of a stale-looking "Brightness: 0%".
- A Wi-Fi network that broadcasts no name (a hidden SSID) tooltips "Wi-Fi: connected (hidden network)" instead of an empty name.
- The terminal's ctrl+click opener only sends `http` and `https` URLs to `xdg-open`, whatever the pty printed: a `file://` span is link-shaped text now, not one click away from executing a local `.desktop` file.
- An OSC 52 clipboard-write flood can no longer hold the keyboard past one deny window: while a prompt is up, further stores are ignored instead of re-arming its timer.
- Two defensive bounds on remote-data paths: a palette index beyond the color table resolves to a theme color instead of panicking, and a ragged cursor cell skips its bake instead of panicking the paint.
- A Koguma move that replaces its target no longer destroys the destination before the move is guaranteed: the old target steps aside and comes back if the rename fails (a cross-filesystem move used to report failure with the destination already gone).
- A Koguma paste that fails partway keeps the undo records of the steps that did complete, so the files moved before the conflict stay undoable.
- Koguma's compress passes file names after `--` for both tar and file-roller, so a downloaded file named like a flag (`-foo`) is archived as a file instead of being parsed as an option, and a failed compress removes the partial archive that would have made every retry fail on "already exists".
- Copying a FIFO or other non-regular file is an honest error instead of a queue wedged forever on a read with no writer, and a directory copy into a mount cycle stops at a depth cap instead of overflowing the stack.
- Koguma's unmount (`gio mount -u`) is bounded at 30 s, so a dead network mount costs a status message instead of a silent wait.
- The single-instance activation socket is created owner-only (0600): with no `XDG_RUNTIME_DIR` the `/tmp` fallback used to leave it world-connectable.
- Koguma skips paths containing raw newlines when saving its state (the line-based format used to tear such a record and silently drop the tab on the next launch).

- A widget manifest's `interval` can no longer crash the shell: nan, a negative, or an overflowing value is a default interval, not a panic on the main thread at boot or on re-discovery.
- A widget's output run is bounded in memory as well as time: stdout is capped at 64 KiB, and a source that backgrounds a pipe-holding grandchild (`sleep 999 & echo hi`) is killed by process group at its deadline instead of wedging the widget's updates forever.
- A widget directory whose `widget.toml` is not a regular file (a FIFO) or is abnormally large is a rejection, not a blocked or byte-hungry discovery pass, and a widget's `click` line no longer inherits the shell's stdout into the journal.
- The msg socket is created owner-only (0600) instead of trusting the runtime directory's permissions, the CLI client bounds its wait and its reply line like the server bounds its peers, request lines are serialized by serde (a URI with a raw control byte no longer tears the line), and a connected-and-silent peer now costs a half-second wait instead of two.
- `bluetoothctl trust` joins its sibling verbs under `timeout`, so a wedged bluetoothd cannot pin the Bluetooth panel busy forever; the nostr mint CLI gets the same 15 s bound.
- Weather and avatar downloads are size-capped (`--max-filesize`), and the avatar is https-only: a broken mirror or a hostile profile field can no longer stream gigabytes into memory or onto disk.
- Night light's one-time `date +%z` read is bounded and no longer panics on empty output.
- Settings, the updates staged flag, and Koguma's state file are written through a temp file and renamed into place: a crash or power loss mid-write tears the temp file, never the config or state the next launch restores from.
- The lock screen no longer accepts any password when a PAM service's stack file exists but carries no `auth` rules (an empty or comment-only `/etc/pam.d/kuma-lock` from a truncated deploy is an empty stack, and an empty stack succeeds): such a service is now skipped like a missing one, and the chain's fall-through still rejects.
- The greeter answers echo-on (visible) PAM questions with an empty string instead of the password, matching the polkit agent's rule; a login module that asks echo-on can no longer echo or log the password.
- Lock-screen password buffers are scrubbed from memory when an attempt, a lock, or an unlock clears them, instead of being left in freed heap for a core dump or swap to recover.
- The polkit helper runs under `timeout -k 5 30`: a helper that hangs and ignores SIGTERM no longer pins the polkit card on "checking..." until the shell is restarted. (Also fixes the stale comment that claimed the setuid helper fallback was not implemented.)
- The greetd socket read is bounded (30 s per exchange): a wedged greetd surfaces as an auth error instead of a greeter stuck on "signing in..." forever.

## v0.5.0 (2026-10-21)

kuma-shell 0.5.0
kuma-files 0.4.1
kuma-term 0.3.1
kuma-monitor 0.2.1

### Added

- `kuma-greeter --version` prints the version line the other apps print, so the upgrade path can pin the greeter's build like the rest of the family.
- Night light's schedule says Always or Scheduled out loud: a segmented switch names the two states the backend always had (Always was reachable only by leaving an endpoint unset, which the dropdowns never showed), Always dims the dropdowns and remembers for the session the times you left, and Scheduled from nothing lands on 9:00 PM to 7:00 AM.
- **User-made widgets.** The bar learns scripts: a widget is a directory (`widget.toml` plus the scripts and icons it names), dropped into `~/.config/kuma-shell/widgets/` or installed from the system pool by the settings page's Available-to-install section. Customs list beside the built-ins, same toggle, same pills. Plain-text stdout is the label, the Waybar custom-module JSON (`text`, `tooltip`, `class`) is the upgrade path, empty output hides the widget; `interval` and a `click` line live in the manifest, and the mode pills and drag ordering work as they do for built-ins. A manifest SVG paints as a glyph in the widget's color, a raster paints as-is. The config follows the filesystem: deleting a widget's directory prunes its placement live, and a reinstall starts from the default position.
- **Kuma Updates chip.** A built-in bar widget (on by default, right cluster) fed by `kuma update --check --json`: icon and moved-package count, hidden while the base is current, accented when security updates are among them. Clicking it opens a panel listing each package with its version change, a severity glyph (critical through low), and an Update now button that stages via polkit; checks run at boot and then every four hours, unprivileged. While the apply runs, the panel shows the build's actual step lines, and a staged result grows a Restart now button while the chip drops its accent (the accent means work waiting on you); the staged result survives a shell restart (a small state file beside `config.toml`, stamped with the boot id of the apply, retired by the reboot into it), so the panel never asks for a second build of the same stage. `kuma-shell msg updates` toggles the panel, the same road the memory probe drives. It probes for the `kuma` CLI at boot, and on anything that is not kumaOS it stays out of the bar, the scheduler, and the settings list entirely: there is nothing to drive. A dismissed password dialog reads as a cancellation, not a failure, and the apply button stands down while a stage waits.

### Changed

- The nostr signer's panel opens on the asks when asks are pending, instead of on whatever tab was last chosen: the thing waiting on a decision outranks the memory, and the remembered tab returns once the queue is clear. A riding nostrconnect:// offer still lands on Pair, as does onboarding.
- The mode pills are per widget, named by the registry. The clock's dial is date+time / time / date (and its default face now shows the weekday and month beside the time), the count chips drop bare text (a chip that reads "5" with no subject is noise), the apps button is icon-only (an action, not a readout), and workspaces and tray have no dial at all. A stored mode outside its widget's dial clamps to the default at load, in memory only: the file stays the portable record.

## v0.4.1 (2026-10-19)

kuma-shell 0.4.1
kuma-files 0.4.1
kuma-term 0.3.1
kuma-monitor 0.2.1

### Changed

- The mode pills are per widget now: the registry names the dial each widget offers, instead of every widget getting icon / icon+text / text. The clock's dial is date+time / time (its shipped face carries the compact day beside the time), the count-bearing chips drop the bare text that read as "5" with no subject, the apps button is icon-only, and the workspaces and tray rows have no dial at all. A config naming a mode a widget does not offer clamps to that widget's default at load, in memory only.
- Idle cost, all four apps: the shell's bar no longer repaints on a sysmon, tray, or vault poll that found nothing moved, and a wedged PipeWire can no longer stall the shell's poll for the session (the volume and stream reads ride timeout now, and the vault widget's poll backs off to a minute when nothing answers). Kamuy skips its repaint while an equal beat applies and while the window cannot paint. Koguma's listing is windowed to the viewport: a directory with tens of thousands of entries builds the rows and cells on screen instead of all of them every frame, renders from a shared snapshot instead of deep-cloning every entry, memoizes the filter order per keystroke, and lands epub chapters without recopying the chapters before them; a rescanned directory reads its listing off the UI thread. kuma-term coalesces a pty output burst into one repaint, and its row cache can no longer be wiped by one scroll burst.

- kuma-term: OSC 52 clipboard writes now ask like kitty's, a small card at the prompt with a content preview, y to allow, n or escape to deny, where before any output reaching the screen could silently rewrite the clipboard. The card denies itself after ten seconds, so an unanswered prompt never holds the keyboard hostage. URLs open on ctrl+click, matching kitty, alacritty, and gnome-terminal; the underline and pointing-hand cursor only show while ctrl is held, and a stray plain click no longer fires the browser.

### Fixed

- kuma-files: epub internals are bounded now, so a hostile or oversized book cannot own the RAM: each zip entry stops reading at 32 MiB, the glance text chain stops at a budget with a marker, and a chapter entity split across a multi-byte character no longer panics the scanner. Archive extraction, compression, and the PDF page-count and preview tools ride `timeout` as well, so a wedged child cannot hold the op queue forever, and a corrupted state file restores at most 32 deduplicated tabs.

- kuma-shell: every subprocess read the poll and panel paths make (volume, media, network, bluetooth, power, brightness, the nostr CLI) is bounded by `timeout` now, so one hung CLI costs seconds instead of a subsystem's updates for the rest of the session. The sway and greetd readers also refuse to allocate what a length prefix alone claims, the greeter's auth exchange is capped, and a workspace name with a quote can no longer break out of the sway command it rides in.

- kuma-monitor: one unreadable sensor entry under /sys/class/hwmon no longer wipes out temperature discovery for the whole session; the entry is skipped instead of aborting the search.

- kuma-term: pasting clipboard text that carries the end-of-bracketed- paste marker no longer hands the shell a command to run; the markers are stripped from the payload. Input writes also moved to a writer thread: a large paste into a program that is not reading (a stopped job, a wedged ssh) used to freeze every tab on the UI thread's blocking write.

- kuma-shell: joining a Wi-Fi network no longer puts the network password on nmcli's command line, where any process could read it in /proc/<pid>/cmdline for the whole connect; it rides a 0600 file in the runtime directory through nmcli's passwd-file. The polkit PAM helper exchange and the per-device bluetoothctl reads are bounded by timeout now, so a hung PAM stack or device cannot pin a worker thread (and an unreaped setuid child) forever.

- kuma-term: a tab whose shell is gone no longer crashes or freezes the terminal. An orphaned background process holding the terminal kept its reader alive after the tab closed, and its events indexed a removed tab (a panic) or landed on the tab that slid into the old slot; events are routed by session identity now, and closing a tab whose child ignores SIGHUP waits for that child on a reaper thread instead of the UI thread, so every tab no longer freezes with it.

- kuma-files: extracting a short-named archive (say, a file called `1.tgz`) crashed with an arithmetic underflow in the folder-name computation, and names like `abc.tgz` extracted into the listing directory itself; the extraction now strips the suffix that actually matched.

- kuma-shell: the keybind socket could be wedged or drained by one local peer. The listener read request lines with no timeout and no length ceiling, so a process that connected and sent nothing (or bytes without a newline) hung every keybind verb until the shell restarted; reads are bounded in time and in size now.

- kuma-shell: the session mirror survives a dead compositor stream. A dropped socket, a restarting compositor, or a reshaped event payload used to end the mirror for the rest of the session (frozen workspaces and window titles until the shell restarted); the stream now reconnects after three seconds, and an event it cannot parse is skipped with a journal line instead of costing the stream.

- kuma-files: Koguma's listing scrolls with the wheel again. The row wrapping the sidebar and the folder column had no min-height floor of zero, so it grew to the folder's full content height instead of the window's: the listing never overflowed and a folder with more files than fit the window had no way to reach the rest.

- kuma-files: folders render in full now. The old 200-entry render cap hid everything past entry 200 even with scrolling working (the icon grid queues thumbnail decodes only for the viewport's neighborhood, so a huge folder no longer queues a decode per cell), and the keyboard cursor (arrows, Home, End) brings its row into view.

## v0.4.0 (2026-10-17)

kuma-shell 0.4.0
kuma-files 0.4.0
kuma-term 0.3.0
kuma-monitor 0.2.0

### Added

- kuma-monitor: Kamuy is single-instance now, the family's convention: launching it while it runs hands a bare poke over the activation socket and exits, and the running window comes forward instead of a twin. The socket itself (the handoff, the stale-socket recovery, the activation pump) graduated into kuma-platform, Koguma's same machinery keyed by app id; Koguma rides it unchanged.

- kuma-term, kuma-files: Higuma and Koguma wear the family's titlebar now, the one Kamuy piloted: the drag region, the centered name (Higuma is the terminal's display name; the binary and app id stay kuma-term), and the minimize, maximize, and close buttons the compositor backs.

### Changed

- kuma-term: Higuma's titlebar now sits one step above the terminal background instead of wearing it outright, pulled toward the theme's own foreground so a pasted kitty theme dresses the bar too. This is the bar treatment Koguma and Kamuy already use, and it reads as a distinct strip on desktops without the wallpaper palette. Nothing to do differently; the lift follows your theme either way.

- kuma-term, kuma-files, kuma-monitor: the titlebar now folds away while the window is tiled and returns when it floats; fullscreen and maximize report the same way and fold too. Nothing to do differently.

### Fixed

- kuma-shell: the settings Ordering page now lets you reorder widgets in every section, left and center included; before, the right column claimed every drop. gpui delivers drag-move events to all three columns' handlers on each move regardless of hover, and the rightmost handler ran last, so it kept overwriting the insertion preview. Each handler now only answers when the cursor is inside its own bounds.

- kuma-files: overwriting an image in place (same path, new contents) now refreshes Koguma's grid thumbnail on the next paint; before, the in-memory thumb cache keyed by path alone and served the old decode until the app restarted. Each cached thumb now carries the mtime it decoded from, the same stamp the disk cache's key already had.

- kuma-shell: the polkit agent's auth prompt now survives polkitd restarts and registers for the logind display session instead of trusting the inherited `XDG_SESSION_ID`, which carries greetd's stale session id for user services; before, one polkitd restart left the machine with no working polkit prompt until the shell restarted.

## v0.3.0 (2026-10-12)

kuma-shell 0.3.0
kuma-files 0.3.0
kuma-term 0.2.0
kuma-monitor 0.1.0

### Added

- **Koguma's Quick Look plays videos.** Opening a video now runs the file through libav in-process (the same decoder Celluloid uses) and presents it frame by frame instead of cycling a short silent loop. Sound plays too, through the sound server (PipeWire's pulse-simple entry point, dlopen'd; systems without it play muted), and the video syncs to the audio clock. Space pauses and resumes a playing clip, and replays one that ran out. The transport lives in a footer strip under the video, the players' convention: elapsed time, a full-width seek bar (click or drag), total time. The Quick Look header slims down to the name, one dim glance (position in the listing, then the file's own facts), the pager, and two buttons; the key-hint line is gone. The keys modal opens with F1 or the header's ? chip (it now also covers Quick Look's rungs), the volume and seek keys stay: J/L seek by ten seconds, +/- steps the volume, M mutes; F11 fullscreens the window while a video is up and escape unwinds it before closing the pane. The standalone viewer plays videos too, with the same engine and the same transport: `kuma-files clip.mp4` opens a playing surface, and the desktop file now claims `video/mp4`, `video/webm`, `video/x-matroska`, `video/quicktime`, and `video/x-msvideo` (audio files wait for the viewer's audio pane and stay with Celluloid). Decode attempts VAAPI hardware first and falls back to software wherever the driver is missing; `KOGUMA_NO_HWACCEL=1` forces software for A/B checks.

- **Koguma renders SVG files.** The grid shows a true-color thumbnail (rasterized in-process at the fit size, so small icons stay crisp), Quick Look glances them, and the standalone viewer opens one with zoom and rotate, the arrows walking sibling SVGs. The desktop claim `image/svg+xml` moves to Koguma, so Enter, double-click, and `xdg-open` all land in it. avif stays unclaimed; it is the last kind with no decoder here.

### Fixed

- **Apps installed mid-session get their icons without a shell restart.** The shell's icon index walked the XDG icon roots once per process, so an app installed while the shell ran (a flatpak, say) resolved no icon in the dock and launcher, and the dock showed its window as a raw entry with the app_id for a label. A named lookup that misses now re-walks the trees when they changed underneath (rate limited to one walk per 5 seconds), and the dock rescans the desktop files when a window matches nothing.

## v0.2.0 (2026-10-10)

kuma-shell 0.2.0
kuma-files 0.2.0
kuma-term 0.2.0

### Added

- **Everything reports its own version.** All three binaries answer `--version` and `-V` with the crate version plus the commit build.sh stamped in (`kuma-shell 0.2.0 (gaab28b8)`); the shell also takes it as a CLI verb, before any session, running instance or IPC, and logs the same line at boot. A build without the stamp falls back to `unknown`, so a plain cargo run still prints. A bug report now pins an exact build, and Koguma's places sidebar signs off with the same tag.

- **The shell's settings gain an About page.** Name, version tag, the host distro read from os-release's `PRETTY_NAME`, and links to the repo and the issue tracker (info.svg joins the icon set). Nothing to configure; the page is static on purpose.

### Changed

- **Koguma's keybinding cheatsheet is a modal.** The sidebar's collapsible Keys block became a Keybindings button that opens a two-column card; escape, the ×, or a click outside the card closes it, and while it is up the key dispatcher ignores everything else. Every hint row was re-checked against the dispatcher and regrouped by task (cursor moves, clipboard, tabs, view, zoom). The saved-state `keys=` line is gone: old state files read it as unknown and drop it silently, so nothing to migrate.

- **Koguma's Network section always shows.** Connect to Server moves into it as the always-present tail row (the plus.svg affordance), so the section is the entry point even with no mounts to list; section rows indent slightly under their headers.

## v0.1.0 (2026-10-09)

kuma-shell 0.1.0
kuma-files 0.1.0
kuma-term 0.1.0

The first release: eight days from an empty repo to the whole desktop. Everything here is new; a reader on kumaOS picks all of it up through the image's kumaui pin.

### Added

- **The desktop is kuma-shell.** One process paints the bar and its widgets, the launcher, the drawer panels, the OSD cards and the settings panel, all layer-shell surfaces on the vendored gpui. Bar widgets are placeable and toggleable from the settings' Widgets and Ordering pages, geometry applies live, and the dock pins favorites beside running windows on its own surface. niri's keybinds drive the shell through `kuma-shell msg` verbs (volume, brightness, media, launcher, notifications) that prefer the socket and fall back to wpctl and brightnessctl standalone.

- **The theme derives from the wallpaper.** The shell extracts a palette from the wallpaper and publishes it to the runtime dir; Koguma reads it opportunistically and recolors on wallpaper changes, with an accent picker for the times the extraction guesses wrong. Wallpaper rotation rides a slideshow with quiet hours.

- **Session state is compositor-neutral.** The workspaces, window title and dock read a session mirror with niri and sway adapters (ADR-0012), so the shell runs end to end on sway, and the displayless smoke exercises the whole surface lifecycle headlessly.

- **The system monitors are widgets with detail panels.** Memory, cpu temperature, disk, microphone and the power profile poll behind an adapter seam; the volume panel carries per-app rows; battery, bluetooth, internet and weather panels hang under the bar with geocoding and a forecast. Audio and brightness changes ride a serialized request queue, and OSD cards name whichever change a key or an outside tool made, do-not-disturb and power profile included, with quiet hours folding into the DND card.

- **The desktop locks, and the greeter is kuma's own.** The lock screen takes logind's Lock signal on every display at once, one shared password field, PAM through a chain that skips uninstalled services (ADR-0008); the idle timeouts live in the settings panel. kuma-greeter speaks greetd, discovers Wayland sessions, and rides the same gpui build as the shell, showing the wallpaper and skipping niri's hotkey overlay; boot-phase timings priced the shell-side login handoff at 16 ms.

- **Displays are set over niri's IPC.** The settings' Displays page applies modes and transforms and persists only the delta into local.kdl (#26), and a failed apply persists nothing (#28).

- **Koguma grew out of a DnD spike into the desktop's file manager.** Tabs, the places sidebar (Home, Recent and Trash up top, then places and mounts), icon and details views, rubber-band select, sortable columns, an op queue with undo and a keyboard-first conflict dialog, rename, compress and extract, open-with over desktop entries, single instance, session restore, and the info rail docked right or bottom. Type-to-filter searches the subtree from a debounced background walk; thumbnails decode three at a time into an atlas that returns its tiles when entries evict.

- **Koguma browses the network.** gvfs and udisks2 mounts under Places, Connect to Server with a protocol picker whose pump relays gio's passwords and host-key questions, disconnect and eject from the right-click menu, and stale bookmarks that stay listed dimmed until a click reconnects them.

- **Quick Look became a reader.** Space peeks a file: text, csv and markdown read whole; epubs scroll as one continuous document with chapter typography and covers; pdfs page through; videos show a poster frame and duration; images zoom, pan and flip with the arrows. Rename opens with the name selected, so typing replaces it.

- **Koguma is the desktop's opener.** Images and PDFs open standalone from Enter or xdg-open, the arrows walk the containing directory's files of the same kind, and the video support grew teeth: libav decodes in-process (feature-gated on the sys crate, a named state when a codec is missing from the system's build), posters and a muted inline preview ride the listing, and the thumbnail pool gained a disk cache.

- **Higuma is the terminal.** alacritty_terminal behind an engine seam, the grid painted as one canvas element, vector box drawing, powerline and braille glyphs, tabs with kitty's jump chords, mouse reporting and selection, scrollback search with a regex toggle, DECSCUSR shapes and blink, a kitty-compatible config at the XDG path with the noctalia pastels built in, a prompt bar tapped from OSC 133/7 with a git segment, `KUMA_TERM=1` for dotfiles, the `-e` argv contract, and URL click-to-open.

- **Everything installs as a desktop app.** build.sh builds in the podman dev container (the host has no toolchain) and installs the shell, Koguma and Higuma with their menu entries, png app icons and the directory-handler binding; the desktop templates ship from the tree, one template, two substitutions.

- **Polkit prompts wear the lock screen's shape.** The shell registers as the polkit authentication agent (#65/#68): one centered card, exclusive keyboard, the identity resolved from unix-user and unix-group. The launcher honors OnlyShowIn/NotShowIn and Hidden with glib's exact semantics (#67), and the app icons paint full-color SVG through the renderer instead of alpha-masked silhouettes.
