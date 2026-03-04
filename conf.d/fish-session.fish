if status is-interactive
    set -l fish_session_cargo_bin "$HOME/.cargo/bin"
    if test -d "$fish_session_cargo_bin"; and not contains -- "$fish_session_cargo_bin" $PATH
        if test -x "$fish_session_cargo_bin/fish-session"; or test -x "$fish_session_cargo_bin/fish-sessiond"
            if functions -q fish_add_path
                fish_add_path -m "$fish_session_cargo_bin"
            else
                set -gx PATH "$fish_session_cargo_bin" $PATH
            end
        end
    end

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
