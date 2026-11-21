# Handoff: kumaOS ships the shell integration snippets for kuma-term

For a kumaOS session. kumaUI (this repo) never edits kumaOS; the owner
lands this change there. The prompt below is the whole job.

Planned to ride the kumaUI 0.10.0 image refresh.

## Why

kuma-term now speaks shell integration (kumaUI issues #114, #115, #113):
the OSC 133 prompt markers, the OSC 7 and OSC 9;9 cwd reports, and the
OSC 7501 program status all land as real features, but only when the
user's shell emits them. The terminal cannot (and must not) inject
scripts into the user's shell startup; the image ships the snippets, so
adoption rides kumaOS and needs no campaign. kitty's model: its shell
integration ships in the distribution's package and loads from a
well-known path.

## The change (kumaOS)

1. Ship two snippet files in the image, for the two shells kumaOS
   defaults matter for:
   - `/etc/profile.d/kuma-term.sh` (POSIX shells: bash, zsh via the
     same file when sourced from .zshenv's login path) and
   - a fish snippet under `/etc/fish/conf.d/kuma-term.fish`.
2. The bash/zsh snippet enables bash's own OSC 133 support and reports
   cwd on every prompt. Bash 5.1+ has native shell integration hooks
   (`PROMPT_COMMAND` with `@`); the portable spelling, which works on
   any bash and zsh:

   ```sh
   # kuma-term shell integration: prompt markers (OSC 133) and cwd
   # reports (OSC 7), consumed by the terminal for cwd-inherited tabs,
   # prompt jumping, and command-finish state. Harmless in other
   # terminals: OSC sequences unset terminals drop.
   __kuma_ps1_mark() {
       case "$?" in
           0) __kuma_code="" ;;
           *) __kuma_code=";$?" ;;
       esac
       printf '\x1b]133;D%s\x07\x1b]133;A\x07' "$__kuma_code"
       printf '\x1b]7;file://%s%s\x07' "$HOSTNAME" "$PWD"
   }
   __kuma_cmd_start() { printf '\x1b]133;B\x07'; }
   __kuma_cmd_output() { printf '\x1b]133;C\x07'; }
   case "$-" in *i*)
       [ "$TERM" = "xterm-kuma-term" ] || [ "$TERM" = "xterm-256color" ] && {
       PROMPT_COMMAND="__kuma_ps1_mark${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
       # B and C ride the DEBUG trap so multi-line commands mark once
       trap '__kuma_cmd_start' DEBUG
       PS0=$'\x1b]133;C\x07'"$PS0"
   } ;; esac
   ```

   The `TERM` gate keeps the sequences off ssh sessions and other
   terminals. `HOSTNAME` must match the machine's hostname (what
   `gethostname` answers): the terminal ignores cwd reports from
   another host, so an ssh session's remote shell cannot steer a local
   tab spawn.
3. The fish snippet does the same with fish's events:

   ```fish
   function __kuma_mark_prompt --on-event fish_prompt
       printf '\x1b]133;D\x07\x1b]133;A\x07'
       printf '\x1b]7;file://%s%s\x07' $hostname $PWD
   end
   function __kuma_mark_cmd --on-event fish_preexec
       printf '\x1b]133;B\x07'
   end
   function __kuma_mark_output --on-event fish_postexec
       printf '\x1b]133;C\x07'
   end
   ```

4. Nothing else: no config knobs, no enable/disable. The snippets gate
   on TERM, and the terminal's own features degrade to no-ops without
   them (no cwd inheritance, no prompt jumps, no status dots).

## Out of scope here

- The OSC 7501 program-status emission (a `working`/`finished`/
  `failed`/`blocked` reporter) is a per-program concern; a tiny
  `kuma-status` helper script could ride a later handoff if anything
  asks for it.
- The notification emission (OSC 9 / OSC 777) needs no image support:
  programs emit it directly.
