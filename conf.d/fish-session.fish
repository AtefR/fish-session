if status is-interactive
    set -l __fish_session_open_bind ctrl-g
    if type -q fish-session
        set -l __fish_session_open_from_config (command fish-session config key open 2>/dev/null)
        if string match -rq '^ctrl-[a-z]$|^ctrl-\]$' -- $__fish_session_open_from_config
            set __fish_session_open_bind $__fish_session_open_from_config
        end
    end

    if not set -q __fish_session_name; and not set -q fish_session_disable_default_bind
        bind -M insert $__fish_session_open_bind fish_session
        bind -M default $__fish_session_open_bind fish_session
    end

    # Inside managed fish-session shells, keep Ctrl-D as delete-char so it
    # does not close the whole session shell when the commandline is empty.
    if set -q __fish_session_name
        bind -M insert \cd delete-char
        bind -M default \cd delete-char
    end
end
