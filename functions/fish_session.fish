function fish_session --description 'Open fish-session picker UI'
    if not type -q fish-session
        echo 'fish_session: fish-session binary not found in PATH.' >&2
        echo 'Install with: cargo install --git https://github.com/AtefR/fish-session.git' >&2
        echo 'Then add cargo bin to fish PATH: fish_add_path ~/.cargo/bin' >&2
        return 127
    end

    if not type -q fish-sessiond
        echo 'fish_session: fish-sessiond binary not found in PATH.' >&2
        echo 'Install with: cargo install --git https://github.com/AtefR/fish-session.git' >&2
        echo 'Then add cargo bin to fish PATH: fish_add_path ~/.cargo/bin' >&2
        return 127
    end

    command fish-session
end
