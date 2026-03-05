function fish_session --description 'Open fish-session picker UI'
    if not type -q fish-session
        echo 'fish_session: fish-session binary not found in PATH.' >&2
        echo 'Install one of:' >&2
        echo '  - Arch (AUR): paru -S fish-session' >&2
        echo '  - Homebrew/Linuxbrew: brew install atefr/tap/fish-session' >&2
        echo '  - Fisher + Cargo: fisher install AtefR/fish-session && cargo install --git https://github.com/AtefR/fish-session.git' >&2
        echo 'Then ensure the binary directory is in fish PATH and open a new shell.' >&2
        return 127
    end

    if not type -q fish-sessiond
        echo 'fish_session: fish-sessiond binary not found in PATH.' >&2
        echo 'Install one of:' >&2
        echo '  - Arch (AUR): paru -S fish-session' >&2
        echo '  - Homebrew/Linuxbrew: brew install atefr/tap/fish-session' >&2
        echo '  - Fisher + Cargo: fisher install AtefR/fish-session && cargo install --git https://github.com/AtefR/fish-session.git' >&2
        echo 'Then ensure the binary directory is in fish PATH and open a new shell.' >&2
        return 127
    end

    command fish-session
end
