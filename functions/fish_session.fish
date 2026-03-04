function fish_session --description 'Open fish-session picker UI'
    if not type -q fish-session
        echo 'fish_session: fish-session binary not found in PATH' >&2
        return 127
    end

    command fish-session
end
