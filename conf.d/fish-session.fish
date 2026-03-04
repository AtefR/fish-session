if status is-interactive
    if not set -q __fish_session_name; and not set -q fish_session_disable_default_bind
        bind -M insert \cg fish_session
        bind -M default \cg fish_session
    end

    # Inside managed fish-session shells, keep Ctrl-D as delete-char so it
    # does not close the whole session shell when the commandline is empty.
    if set -q __fish_session_name
        bind -M insert \cd delete-char
        bind -M default \cd delete-char
    end
end
