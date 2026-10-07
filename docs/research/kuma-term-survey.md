# Terminal emulator feature survey for kuma-term

2026-10-07

Primary-source feature survey of popular Linux terminal emulators, written to inform
the design of kuma-term, the gpui-based terminal that will replace kitty in kumaOS.
Constraints that shape every recommendation below: Wayland/niri only, fish as the
login shell, single OS window, no per-app config files, palette delivered from the
shell at runtime.

Method: every claim is cited inline to a primary source (project docs, config
references, changelogs, or release notes) fetched on 2026-10-06 and 2026-10-07.
Where a source is the vendor's own benchmark or claim, that is stated. Cells in the
comparison table marked n/s were not seen in the sources cited here; absence of
evidence is not evidence of absence.

## kitty

Sources: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>, <https://sw.kovidgoyal.net/kitty/graphics-protocol/>, <https://sw.kovidgoyal.net/kitty/underlines/>, <https://sw.kovidgoyal.net/kitty/performance/>, <https://sw.kovidgoyal.net/kitty/conf/>, <https://sw.kovidgoyal.net/kitty/protocol-extensions/>, <https://sw.kovidgoyal.net/kitty/clipboard/>, <https://sw.kovidgoyal.net/kitty/changelog/>, <https://sw.kovidgoyal.net/kitty/shell-integration/>

- Defines the de facto modern protocol stack the whole industry now implements.
- Keyboard protocol: a full replacement for legacy encoding with five progressive
  enhancement flags (disambiguate escape codes, report event types, report alternate
  keys, report all keys as escape codes, report associated text), a query mechanism,
  and a long argument for why xterm modifyOtherKeys should not be used
  <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.
- Own protocol family beyond the keyboard: the graphics protocol (cell-placed
  images, RGB/RGBA/PNG payloads, optional compression, animation, storage quotas
  <https://sw.kovidgoyal.net/kitty/graphics-protocol/>), text sizing, drag and drop,
  multiple cursors, file transfer, desktop notifications, pointer shapes, DECCARA
  region styling, unscroll, color stack (OSC 21), and a clipboard protocol that
  extends OSC 52 to arbitrary data types with a permission model
  <https://sw.kovidgoyal.net/kitty/protocol-extensions/>, <https://sw.kovidgoyal.net/kitty/clipboard/>.
- Styled and colored underlines (straight, double, curly, dotted, dashed, plus
  underline color), with render-level tuning via undercurl_style (thin/thick,
  sparse/dense) <https://sw.kovidgoyal.net/kitty/underlines/>, <https://sw.kovidgoyal.net/kitty/conf/>.
- Publishes benchmarks on its own performance page: throughput 134.55 MB/s average
  for kitty vs 54.05 for alacritty, 48.50 for wezterm, 61.83 for gnome-terminal;
  CPU-seconds tables for the same workload; and the note that konsole,
  gnome-terminal, and xterm do not support the synchronized update escape code
  (<https://gitlab.com/gnachman/iterm2/-/wikis/synchronized-updates-spec>) and would
  improve 20 to 50 percent if they did. Same page admits kitty inserts artificial
  repaint_delay and input_delay sleeps to bound CPU and power use, which is a
  latency-for-power trade a kuma-term does not have to make
  <https://sw.kovidgoyal.net/kitty/performance/>.
- Scrollback: scrollback_lines (default 2000) kept in memory with on-demand
  allocation, overflow handed to an external pager via scrollback_pager with
  scrollback_pager_history_size for persistent pager history
  <https://sw.kovidgoyal.net/kitty/conf/>.
- kitty.conf defines no bold-to-bright option: bold changes font weight only, the
  16-color table simply has dull and bright variants <https://sw.kovidgoyal.net/kitty/conf/>.
- Shell integration (OSC 133 style prompt marking, output browsing, marked scrollback)
  shipped as opt-in scripts <https://sw.kovidgoyal.net/kitty/shell-integration/>.
- UX: tabs plus window layouts (splits, grid, tall, stack), sessions, a scrollbar
  (0.43), vertical tabs (0.48), custom shaders (0.49), multiple cursors (0.43), drag
  and drop (0.47) <https://sw.kovidgoyal.net/kitty/changelog/>.
- Fonts: choose-fonts kitten UI, variable fonts, per-face bold/italic selection
  <https://sw.kovidgoyal.net/kitty/conf/>.
- Reputation: the protocol innovator (everyone implements kitty's specs), and the
  benchmark-lazy (repaint delay) battery hog by its own admission.

## Ghostty

Sources: <https://ghostty.org/docs/about>, <https://ghostty.org/docs/features>, <https://ghostty.org/docs/config/reference>, <https://ghostty.org/docs/vt/reference>, <https://ghostty.org/docs/vt/external>

- Stated goals: fast, feature-rich, native UI. Architecture: libghostty core with a
  GTK4 frontend on Linux (native components per platform), written in Zig
  <https://ghostty.org/docs/about>.
- Developer-facing protocol support is explicitly advertised: kitty graphics
  protocol, kitty keyboard protocol, synchronized rendering, light/dark mode
  notifications <https://ghostty.org/docs/features/>.
- VT reference documents OSC 4 and OSC 10 through 19 color query/change, OSC 7
  (cwd), OSC 8 (hyperlinks), OSC 9 (notifications), OSC 21 (kitty color protocol),
  OSC 22 (pointer shape), OSC 52 (clipboard), plus the reset family
  <https://ghostty.org/docs/vt/reference>. The external-protocols page commits to
  following the protocol-origin terminal's exact behavior (kitty for kitty specs)
  <https://ghostty.org/docs/vt/external>.
- Config reference details worth stealing: scrollback-limit is specified in bytes,
  in-memory only, per surface (a cleaner model than line counts); image-storage-limit
  caps kitty graphics memory; grapheme-width-method chooses between wcwidth and
  Unicode width tables; clipboard handling is "allow reading after prompting the
  user and allow writing unconditionally" via OSC 52
  <https://ghostty.org/docs/config/reference>.
- Bold handling: default bold keeps its color; the old bold-is-bright behavior is
  deprecated as of 1.2 with a separate bold-color option
  <https://ghostty.org/docs/config/reference>.
- Ligatures supported with per-font-feature toggles; GPU rendering is OpenGL on
  Linux; native tabs and splits <https://ghostty.org/docs/features>.
- Publishes no benchmark numbers, only the claim that it aims to be in the same
  class as the fastest terminals <https://ghostty.org/docs/about>.
- Reputation: fastest-rising terminal of the decade, protocol-complete, but its docs
  are young (the VT reference is self-described as work-in-progress).

## Alacritty

Sources: <https://raw.githubusercontent.com/alacritty/alacritty/master/README.md>, <https://raw.githubusercontent.com/alacritty/alacritty/master/docs/features.md>, <https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>

- Philosophy: minimalism, correctness, and speed via OpenGL; no tabs or splits by
  design, window management is left to the WM or a multiplexer; vi mode, hints
  (keyboard selection of URLs and text patterns), and scrollback search are the
  main UX features <https://raw.githubusercontent.com/alacritty/alacritty/master/docs/features.md>.
- CHANGELOG facts that contradict common lore:
  - Kitty keyboard protocol supported since 0.13.0 (the belief that Alacritty lacks
    it is out of date).
  - Synchronized output: legacy DCS variant since 0.8.0, standard CSI 2026 since
    0.13.0.
  - Undercurl plus underline color since 0.11.0, inline IME since 0.11.0.
  - OSC 8 hyperlinks since 0.11.0.
  - OSC 52 paste is disabled by default since 0.13.0 (clipboard-injection caution).
  - draw_bold_text_with_bright_colors defaults to false since 0.4.2.
  - Fractional Wayland scaling since 0.12.0.
  <https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>
- No image protocol of any kind has ever landed (no graphics entry in the changelog).
- Reputation: the performance-and-stability workhorse; feature additions are slow
  and conservative; TOML config, no runtime reflow of long lines (known limitation
  acknowledged in its docs).

## WezTerm

Sources: <https://wezterm.org/features.html>, <https://wezterm.org/config/lua/config/bold_brightens_ansi_colors.html>, <https://wezterm.org/config/lua/config/front_end.html>, <https://wezterm.org/config/lua/config/enable_kitty_keyboard.html>, <https://wezterm.org/config/lua/config/enable_csi_u_key_encoding.html>, <https://wezterm.org/config/lua/config/automatically_reload_config.html>

- The feature list is the widest of the survey: ligatures, OSC 8 hyperlinks,
  searchable scrollback, kitty graphics and sixel images, tabs, splits, and a
  unique built-in multiplexer (SSH and serial domains, attach/detach, panes that
  survive process death) <https://wezterm.org/features.html>.
- Configuration is a full Lua runtime with hot reload by default
  (automatically_reload_config), which is the most powerful and the heaviest config
  model in the survey <https://wezterm.org/config/lua/config/automatically_reload_config.html>.
- bold_brightens_ansi_colors defaults to true ("BrightAndBold"): WezTerm is the
  outlier that brightens the 16 ANSI colors on bold by default, inherited from the
  classic xterm behavior <https://wezterm.org/config/lua/config/bold_brightens_ansi_colors.html>.
- Renderer: front_end defaults to OpenGL; WebGpu (Vulkan on Linux) is optional, and
  a Software fallback exists <https://wezterm.org/config/lua/config/front_end.html>.
- Keyboard: kitty keyboard protocol support is a config option
  (enable_kitty_keyboard) and legacy CSI u encoding has its own toggle
  (enable_csi_u_key_encoding) <https://wezterm.org/config/lua/config/enable_kitty_keyboard.html>, <https://wezterm.org/config/lua/config/enable_csi_u_key_encoding.html>.
- Reputation: the everything-terminal (mux, Lua, sixel, graphics), respected but
  heavy; the one to study for features, not for restraint.

## foot

Sources: <https://codeberg.org/dnkl/foot>, <https://codeberg.org/dnkl/foot/raw/branch/master/doc/benchmark.md>, <https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>

- Wayland-native only, no X11 backend; IME via zwp_text_input_v3; fractional
  scaling support with a long written rationale; server mode (one process, many
  windows); keyboard-driven URL mode and scrollback search; full OSC list
  (8 hyperlinks, 52 clipboard, 133 prompt marking, 7 cwd, and more)
  <https://codeberg.org/dnkl/foot>.
- Publishes its own vtebench comparison (foot ahead of alacritty on the measured
  workloads, 2022 data) <https://codeberg.org/dnkl/foot/raw/branch/master/doc/benchmark.md>.
- The changelog is the best public record of modern protocol adoption curves:
  - 1.6.0: IME support (compile-time optional) plus a DECSET (CSI ? 737769) to let
    applications toggle the IME, e.g. around vim insert mode.
  - 1.16.0: grapheme cluster processing (mode 2027), fine-grained surface damage.
  - 1.18.0: styled and colored underlines, SGR 21 double underline, XTPUSHCOLORS
    color palette stack, in-band resize notifications (mode 2048), kitty desktop
    notifications (OSC 99), high-resolution wheel scrolling.
  - 1.21.0: kitty text-sizing protocol (OSC 66), gamma-correct blending behind
    wp_color_management_v1, user-defined regexes for hints.
  - 1.23.0: dark/light mode detection (mode 2031), OSC 52 support advertised in DA.
  - 1.25.0: SHM buffers page-aligned and stride-256-byte-aligned so the compositor
    can import them directly as GPU textures; measurable latency drop.
  - 1.26.0: colors-dark/colors-light theme sections with switch bindings,
    optional background blur via ext-background-effect-v1.
  - 1.27.0/1.28.0: configurable URL underline style, terminal visibility reports
    via xdg_toplevel suspended state.
  <https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>
- Bold-to-bright exists only as an opt-in tweak (bold-text-in-bright-amount),
  default off <https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>.
- Renders on the CPU into shared-memory buffers rather than GPU-drawing glyphs;
  the win is zero-copy compositing and tiny idle footprint, the cost is a fast CPU
  path rather than shader work.
- Reputation: the reference Wayland citizen; minimal, fast, and the terminal whose
  issues other Wayland terminals get linked to.

## Rio

Sources: <https://rioterm.com/docs/config>

- Rust terminal with its own renderer (Sugarloaf): native Vulkan on Linux (also
  DX12/Metal on other platforms) with a tiny-skia CPU fallback
  <https://rioterm.com/docs/config>.
- TOML config with automatic reload; tabs and splits built in; ligatures via
  fonts.features; hint patterns configurable; DEC 2027 grapheme clustering;
  draw-bold-text-with-light-colors defaults to false; shell integration can be
  injected into the spawned shell; kitty graphics protocol supported
  <https://rioterm.com/docs/config>.
- Reputation: young and stylish, less battle-tested than the others here; notable
  as the second Vulkan terminal and for shipping a single-binary UX.

## Contour

Sources: <https://raw.githubusercontent.com/contour-terminal/contour/master/README.md>

- OpenGL 3.3 renderer; sixel and ReGIS images; OSC 8 and OSC 52; synchronized
  output (CSI 2026); text reflow (DEC 2028); YAML config with live reload;
  ligatures; truecolor <https://raw.githubusercontent.com/contour-terminal/contour/master/README.md>.
- Distinctive: persistent sessions via a daemon, designed to interoperate with
  tmux rather than replace it <https://raw.githubusercontent.com/contour-terminal/contour/master/README.md>.
- Reputation: the standards nerd's terminal (its maintainer documents VT
  extensions on contour-terminal.org), modest user base, strong protocol coverage.

## Konsole and gnome-terminal (brief)

Sources: <https://konsole.kde.org/>, <https://raw.githubusercontent.com/GNOME/gnome-terminal/master/README.md>, <https://sw.kovidgoyal.net/kitty/performance/>, <https://fishshell.com/docs/current/relnotes.html>

- Konsole: Qt/KDE terminal, tabs and profiles, search, bookmarks, silence and
  activity monitoring <https://konsole.kde.org/>. Fish 4.9.0 disables its kitty
  keyboard protocol requests on Konsole because Konsole's implementation is buggy,
  and fish 4.8.1 disabled OSC 133 prompt marking there for the same reason; these
  are per-terminal bug workarounds living in the shell
  <https://fishshell.com/docs/current/relnotes.html>.
- gnome-terminal: built on the VTE widget and runs as a single D-Bus activated
  server process (windows are clients of that server)
  <https://raw.githubusercontent.com/GNOME/gnome-terminal/master/README.md>. It is
  the throughput baseline in kitty's benchmark table (61.83 MB/s) and lacks the
  synchronized update escape code <https://sw.kovidgoyal.net/kitty/performance/>.
- Both matter here mainly as counter-examples: giant install bases, slow protocol
  adoption, and (for Konsole) implementations so broken that the shell carries
  anti-workarounds.

## Protocol support comparison

n/s means not seen in the sources cited for that terminal in this document.

| Terminal | Kitty keyboard protocol | Synchronized output | OSC 8 | OSC 52 | Images | Styled underlines | Bold brightens by default |
|---|---|---|---|---|---|---|---|
| kitty | Yes, it defines it (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>) | Yes (<https://sw.kovidgoyal.net/kitty/performance/>) | Yes (<https://sw.kovidgoyal.net/kitty/conf/>) | Yes, extended (<https://sw.kovidgoyal.net/kitty/clipboard/>) | Own graphics protocol (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>) | Yes (<https://sw.kovidgoyal.net/kitty/underlines/>) | No option exists (<https://sw.kovidgoyal.net/kitty/conf/>) |
| Ghostty | Yes (<https://ghostty.org/docs/features>) | Yes (<https://ghostty.org/docs/features>) | Yes (<https://ghostty.org/docs/vt/reference>) | Yes (<https://ghostty.org/docs/vt/reference>) | Kitty graphics, capped (<https://ghostty.org/docs/config/reference>) | n/s | No, bold keeps color (<https://ghostty.org/docs/config/reference>) |
| Alacritty | Yes, since 0.13.0 (<https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>) | Yes, CSI 2026 since 0.13.0 (<https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>) | Yes, since 0.11.0 (<https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>) | Yes, paste off by default (<https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>) | None | Yes, since 0.11.0 (<https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>) | No, since 0.4.2 (<https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>) |
| WezTerm | Yes, option-gated (<https://wezterm.org/config/lua/config/enable_kitty_keyboard.html>) | n/s | Yes (<https://wezterm.org/features.html>) | n/s | Kitty graphics and sixel (<https://wezterm.org/features.html>) | n/s | Yes, the outlier (<https://wezterm.org/config/lua/config/bold_brightens_ansi_colors.html>) |
| foot | Yes (<https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>) | Yes (<https://codeberg.org/dnkl/foot>) | Yes (<https://codeberg.org/dnkl/foot>) | Yes, gated by security.osc52 (<https://codeberg.org/dnkl/foot>) | Sixel (<https://codeberg.org/dnkl/foot>) | Yes, 1.18.0 (<https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>) | Opt-in tweak only (<https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>) |
| Rio | n/s | n/s | n/s | n/s | Kitty graphics (<https://rioterm.com/docs/config>) | n/s | No (<https://rioterm.com/docs/config>) |
| Contour | n/s | Yes (<https://raw.githubusercontent.com/contour-terminal/contour/master/README.md>) | Yes (<https://raw.githubusercontent.com/contour-terminal/contour/master/README.md>) | Yes (<https://raw.githubusercontent.com/contour-terminal/contour/master/README.md>) | Sixel and ReGIS (<https://raw.githubusercontent.com/contour-terminal/contour/master/README.md>) | n/s | n/s |
| Konsole | Buggy, fish disables it there (<https://fishshell.com/docs/current/relnotes.html>) | No (<https://sw.kovidgoyal.net/kitty/performance/>) | n/s | n/s | n/s | n/s | n/s |
| gnome-terminal | n/s | No (<https://sw.kovidgoyal.net/kitty/performance/>) | n/s | n/s | n/s | n/s | n/s |

Reading of the table: the kitty keyboard protocol and CSI 2026 synchronized output
are now table stakes among the modern terminals (kitty, Ghostty, Alacritty, foot,
WezTerm), OSC 8 and OSC 52 are universal in that group, and the real differentiator
is images, where kitty's graphics protocol has won over sixel everywhere except
foot and Contour. Vendor-run benchmarks (kitty's and foot's) should be read as
advertisements with methodology attached, but the kitty numbers at least come with
reproducible scripts <https://sw.kovidgoyal.net/kitty/performance/>.

## What kuma-term should copy, defer, and skip

Context recap: kuma-term runs under kumaOS, Wayland/niri only, fish as the shell,
one OS window managed by the shell, no per-app config file, palette pushed from the
shell, rendering via gpui.

### MVP (copy now)

- Kitty keyboard protocol, disambiguate mode on by default, with the full flag set
  reachable by applications: fish 4.0.0 requests it (and modifyOtherKeys) on every
  prompt, so this is the single highest-leverage input feature
  (<https://fishshell.com/docs/current/relnotes.html>). Reason: fish expects it, and
  every modern terminal already speaks it.
- Synchronized output (CSI 2026): one parser state bit plus a deferred frame flush.
  Reason: cheap, eliminates redraw tearing in full-screen apps, and kitty's own
  numbers say its absence costs 20 to 50 percent in redraw-heavy workloads
  (<https://sw.kovidgoyal.net/kitty/performance/>).
- OSC 8 hyperlinks plus URL detection with a keyboard URL mode in foot's style:
  hints and jump labels, no mouse required (<https://codeberg.org/dnkl/foot>).
  Reason: tiny code, and pointer-free navigation is the kumaOS default interaction
  model.
- Styled and colored underlines per kitty's spec: the format is a de facto standard
  implemented by kitty, Ghostty, Alacritty, and foot
  (<https://sw.kovidgoyal.net/kitty/underlines/>). Reason: LSP diagnostics depend
  on it and it is nearly free in a glyph renderer.
- Color surface for the palette-from-shell design: OSC 4 and OSC 10/11/12 queries
  and change, OSC 11 reporting, modeled on Ghostty's documented set
  (<https://ghostty.org/docs/vt/reference>). Reason: kuma-term's palette arrives
  from the shell, so the query/change surface is the config system.
- OSC 52 clipboard with an explicit security policy, foot's security.osc52 gating
  or Ghostty's read-after-prompt and write-unconditional default
  (<https://codeberg.org/dnkl/foot>, <https://ghostty.org/docs/config/reference>).
  Reason: required by fish and remote workflows, dangerous enough to need the
  policy switch Alacritty and foot both ship.
- Damage-driven rendering with no artificial repaint or input delays: kitty adds
  sleeps for power reasons, foot instead does fine-grained damage and page-aligned
  buffers (<https://sw.kovidgoyal.net/kitty/performance/>,
  <https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>). Reason: gpui is
  already damage-aware; the terminal should be the well-behaved citizen on niri.
- Bounded in-memory scrollback with search, measured in bytes like Ghostty's
  scrollback-limit (<https://ghostty.org/docs/config/reference>). Reason: a byte
  cap matches how kumaOS budgets memory better than line counts do.
- IME via zwp_text_input_v3, following foot's implementation (including an
  application-toggleable private mode) (<https://codeberg.org/dnkl/foot>,
  <https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>). Reason: niri
  supports text-input-v3 and fish users with CJK input should not be second-class.

### Later (defer, design for)

- Kitty graphics protocol subset with a memory cap, Ghostty's
  image-storage-limit model (<https://ghostty.org/docs/config/reference>). Reason:
  image previews are nice, but nothing in kumaOS daily driving depends on them on
  day one.
- Kitty text sizing protocol (OSC 66): foot needed one release to add it
  (<https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>). Reason: small
  and useful for headlines in TUIs, not core.
- Grapheme clustering (mode 2027) and DEC 2028 text reflow: correctness wins for
  emoji and CJK, but they touch every cell code path. Reason: high churn, schedule
  after the core grid is stable.
- Dark/light mode notifications (mode 2031) and theme switching: foot's
  colors-dark/colors-light plus 2031 is the right shape
  (<https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>). Reason: kumaOS
  gets its palette from the shell anyway, so this folds into that work later.
- Shell integration UI on OSC 133: fish already emits OSC 133 and OSC 7 every
  prompt (<https://fishshell.com/docs/current/relnotes.html>). Reason: enables
  output navigation and prompt-aware scrollback, but Konsole's experience shows the
  implementation must be validated against fish, not the other way around.- OSC 99 desktop notifications and the xdg_toplevel suspended visibility reports
  foot added in 1.28.0 (<https://codeberg.org/dnkl/foot/raw/branch/master/CHANGELOG.md>).
  Reason: pleasant, not blocking.
- Pager-backed overflow history beyond the memory cap, kitty's scrollback_pager
  model (<https://sw.kovidgoyal.net/kitty/conf/>). Reason: only matters for very
  long sessions.

### Never (for kumaOS)

- Tabs and splits inside the terminal: kitty, Ghostty, and WezTerm all grew these
  because their users lack a good WM; kumaOS has niri and the kuma shell doing
  windowing. Reason: duplicated responsibility, guaranteed to fight tiling.
- A config file system of any kind: kitty.conf, WezTerm's Lua runtime, Contour's
  YAML, and Rio's TOML all exist to let users override defaults kumaOS owns.
  Reason: kuma-term's configuration is the shell (palette, fonts via environment
  and escape sequences), so there is nothing for a file to say.
- Multiplexer, daemon, or persistent sessions (WezTerm's mux, Contour's daemon,
  foot's server mode): kuma-term is one window per service lifecycle. Reason:
  kuma-shell already owns persistence and layout.
- Sixel and ReGIS: legacy image paths; kitty graphics is the modern protocol the
  ecosystem standardized on. Reason: dead formats, real code cost.
- X11 backend, macOS, Windows: kumaOS is Wayland/niri only. Reason: portability is
  not a goal, and Wayland-only removes an entire input and DPI compatibility layer.
- Ligatures: Rio, WezTerm, and Contour ship them and kitty makes them optional;
  programming-ligature demand inside kumaOS workflows is minimal. Reason: font
  shaping complexity is the worst cost-to-benefit ratio in this survey. Revisit
  only if the user misses them.

## Prompt UX (the starship angle)

Starship (<https://starship.rs/>) is prompt-side, not terminal-side: a cross-shell
prompt engine that runs inside any terminal (fish support is `starship init fish |
source`) and needs only truecolor and Nerd Font glyphs from the terminal. Its
features therefore arrive in kuma-term for free; nothing to build.

The terminal-side counterpart of that UX is shell integration, OSC 133 prompt
marking plus OSC 7 cwd reporting, which fish already emits every prompt
(<https://fishshell.com/docs/current/relnotes.html>). Because kumaOS owns both ends
(shell and terminal), kuma-term can go past what a prompt alone can do:

- Prompt-aware scrollback: jump between prompts, a sticky prompt line while
  scrolled, click a prompt to select its whole output block.
- Output blocks: collapse long command output behind a one-line summary
  (duration, exit status), expand on click.
- A scrollback gutter mark per command, colored by exit status where the shell
  reports it.
- cwd awareness via OSC 7, so the launcher and dock can inherit the terminal's
  directory.
- OSC 9/777 notifications routed to kuma-shell's notification daemon (long
  command finishes, the shell toasts it).
- Smart selection: double-click selects the whole path, URL, or git SHA.

Prerequisite that belongs in the MVP font stack: Nerd Font fallback coverage, so
powerline glyphs and starship's icons never render as tofu. Parsing OSC 133 is
cheap and should land with the MVP parser even though the block UI above is
later work; the marks are load-bearing scrollback structure once they exist.

## Amendment (2026-10-07): portable scope and the built-in prompt bar

Two scope decisions landed after the survey was written, and they amend the
conclusions above.

### Portability: any distro, kuma-shell optional

kuma-term is not a kumaOS exclusive. It runs on any Linux distro. What changes:

- The "Never: a config file" decision above is rescinded. Portability needs a
  fallback for the palette and fonts, so: a minimal TOML config
  (`~/.config/kuma-term/config.toml`, the kuma-shell house pattern) carrying
  theme, font, and a few toggles, with sane built-in defaults underneath. OSC
  4/10/11 queries and mode 2031 dark/light remain runtime surfaces on top of it.
  On kumaOS the shell's palette is the theme's source, so the config file
  becomes the portable fallback rather than the primary system.
- The "Never: X11" decision softens: gpui ships both Wayland and X11 backends,
  and a portable terminal cannot refuse half the distros out there. Wayland
  stays the preferred path; X11 comes nearly free from the framework.
- Notifications: OSC 9/777 renders as a desktop notification over
  org.freedesktop.Notifications (every distro has a daemon). On kumaOS that
  daemon is kuma-shell, and integration extras (cwd-aware launcher, dock
  inheritance) are progressive enhancements detected at runtime, never runtime
  dependencies.
- Tabs/splits, multiplexer/daemon, sixel: stay Never (any WM or tmux covers
  them, and they are portability-neutral).

### The starship features, built in

The owner wants starship's value built into the terminal, not layered on top as
a prompt config. The prompt line itself stays the shell's (fish draws it); what
a terminal can own is the ambient context around it, and it can compute all of
it itself:

- cwd, from OSC 7 (fish sends it by default).
- Command boundaries, duration, and exit status, from OSC 133 (fish marks
  prompts unconditionally and its D mark carries the exit status by default
  (<https://sw.kovidgoyal.net/kitty/shell-integration/>,
  <https://fishshell.com/docs/current/terminal-compatibility.html>)).
- Git branch and dirty state, read from the cwd (git2).
- Detected toolchains, from project files in the cwd (Cargo.toml, package.json,
  pyproject.toml, go.mod, .tool-versions, mise.toml).
- System load, from /proc (the sysmon recipe).

Rendered two ways: a slim status bar along the window's edge (the iTerm2 status
bar model) and the scrollback gutter (per-command marks, green or red by exit
status). On fish this is zero-config: fish already reports everything the bar
needs. On bash and zsh, an optional shell-integration script (the kitty model)
supplies the same marks, so the feature degrades rather than dies on other
shells. This is the headline differentiator: no terminal in the survey computes
starship-class context itself, and it needs no prompt framework, no Nerd Font
config on the user's side, and works on any distro.

## The three most surprising findings

1. Alacritty has supported the kitty keyboard protocol since 0.13.0 (and CSI 2026
   synchronized output since the same release). The widespread claim that
   Alacritty lacks the kitty keyboard protocol is simply out of date; the CHANGELOG
   is unambiguous (<https://raw.githubusercontent.com/alacritty/alacritty/master/CHANGELOG.md>).
2. WezTerm is the only surveyed terminal that brightens ANSI colors on bold by
   default (bold_brightens_ansi_colors defaults to true). Kitty has no such option,
   Alacritty flipped its default to false in 0.4.2, Ghostty deprecated bold-is-bright
   in 1.2, Rio defaults it off, and foot keeps it behind an opt-in tweak. "Bold
   means bright" is legacy xterm behavior surviving in exactly one modern terminal
   (<https://wezterm.org/config/lua/config/bold_brightens_ansi_colors.html>).
3. fish ships per-terminal anti-workarounds: it disables its kitty keyboard
   protocol requests on Konsole because Konsole's implementation is buggy (4.9.0)
   and disabled OSC 133 prompt marking there a release earlier for the same reason.
   Protocol support is ecosystem politics, not just spec compliance: a terminal can
   implement a spec badly enough that the shell blacklists it
   (<https://fishshell.com/docs/current/relnotes.html>).
