function fish_session --description 'Open fish-session picker UI'
    set -l cargo_bin "$HOME/.cargo/bin"

    if not type -q fish-session
        if test -x "$cargo_bin/fish-session"
            if not contains -- "$cargo_bin" $PATH
                if functions -q fish_add_path
                    fish_add_path -m "$cargo_bin"
                else
                    set -gx PATH "$cargo_bin" $PATH
                end
            end
        else
            echo 'fish_session: fish-session binary not found. Install with: cargo install --git https://github.com/AtefR/fish-session.git' >&2
            return 127
        end
    end

    if not type -q fish-sessiond
        if test -x "$cargo_bin/fish-sessiond"
            if not contains -- "$cargo_bin" $PATH
                if functions -q fish_add_path
                    fish_add_path -m "$cargo_bin"
                else
                    set -gx PATH "$cargo_bin" $PATH
                end
            end
        else
            echo 'fish_session: fish-sessiond binary not found. Install with: cargo install --git https://github.com/AtefR/fish-session.git' >&2
            return 127
        end
    end

    command fish-session
end
