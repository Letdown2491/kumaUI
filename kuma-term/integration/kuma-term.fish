# kuma-term prompt, opt-in: kuma-term sets KUMA_TERM=1 in the environment it
# gives the shell, so this stays inert in kitty and every other terminal.
if status is-interactive; and set -q KUMA_TERM
    if set -q KUMA_TERM_BAR
        # bar-as-the-prompt: the bar at the top of the window carries the
        # path, branch, exit code, and duration, so the in-grid prompt is
        # one bold marker cell. It turns red after a failed command, which
        # is the one cue worth having at the typing point itself.
        function fish_prompt --description 'kuma-term prompt'
            set -l code $status
            if test $code -ne 0
                set_color red --bold
            else
                set_color --bold
            end
            printf '❯ '
            set_color normal
        end
    else
        # Same shape as fish's default prompt minus user@host: abbreviated
        # cwd, git branch, no name.
        function fish_prompt --description 'kuma-term prompt'
            set -l branch (git -C $PWD branch --show-current 2>/dev/null)
            set_color $fish_color_cwd
            printf '%s' (prompt_pwd)
            set_color normal
            if test -n "$branch"
                printf ' (%s)' $branch
            end
            printf '> '
        end
    end

    # OSC 133 lifecycle for kuma-term's prompt bar: prompt shown (A),
    # command executing (C), command finished with $status (D). fish emits
    # OSC 7 (cwd) and the 133;A/B pair on its own; C/D it does not, so the
    # bar gets its exit code and duration from here.
    function __kuma_term_prompt_mark --on-event fish_prompt
        printf '\e]133;A\e\\'
    end
    function __kuma_term_preexec_mark --on-event fish_preexec
        printf '\e]133;C\e\\'
    end
    function __kuma_term_postexec_mark --on-event fish_postexec
        printf '\e]133;D;%s\e\\' $status
    end
end
