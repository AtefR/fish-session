# fish-session

UI-first session manager for Fish shell with persistent PTY sessions, fast reattach, and an in-terminal picker.

## Features

- Persistent shell sessions via `fish-sessiond`
- Floating-style in-terminal picker via `fish-session`
- Default Fish keybinding: `Ctrl-G`
- Zoxide mode (`Ctrl-O`) for directory-based session create/attach
- Session status chip at the bottom-left while attached
- Reattach with scrollback replay

## Installation

Choose one installation path.

### Arch Linux (AUR)

```bash
paru -S fish-session
# or latest git version:
paru -S fish-session-git
```

This installs both binaries and Fish integration files. Fisher is not required on this path.

### Homebrew / Linuxbrew

```bash
brew tap atefr/tap
brew install atefr/tap/fish-session
```

This installs both binaries and Fish integration files. Fisher is not required on this path.

### Other systems (Fisher + Cargo)

1) Install Fish integration with Fisher:

```fish
fisher install AtefR/fish-session
```

2) Install binaries:

```bash
cargo install --git https://github.com/AtefR/fish-session.git
```

3) Ensure Cargo bin is in Fish `PATH`:

```fish
fish_add_path ~/.cargo/bin
```

4) Open a new Fish shell.

### Other systems (Fisher + GitHub release binaries)

1) Install Fish integration with Fisher:

```fish
fisher install AtefR/fish-session
```

2) Download binaries from the latest release assets:

```bash
VERSION=v0.1.3
curl -fL -o fish-session "https://github.com/AtefR/fish-session/releases/download/${VERSION}/fish-session"
curl -fL -o fish-sessiond "https://github.com/AtefR/fish-session/releases/download/${VERSION}/fish-sessiond"
install -Dm755 fish-session ~/.local/bin/fish-session
install -Dm755 fish-sessiond ~/.local/bin/fish-sessiond
```

3) Ensure local bin is in Fish `PATH`:

```fish
fish_add_path ~/.local/bin
```

4) Open a new Fish shell.

If release assets are not available yet, use the AUR or Cargo installation path.

## Quick Start

1. Open picker: `Ctrl-G`
2. Create session: `Ctrl-N`, type name, `Enter`
3. Attach selected session: `Enter`
4. Detach active session: `Ctrl-]`

## Keybindings

### Session Picker

- `Enter`: attach selected session
- `Ctrl-N`: create session
- `Ctrl-D`: delete selected session
- `Ctrl-R`: rename selected session
- `Ctrl-O`: open zoxide mode
- `Esc`: close picker (or clear search when search is not empty)

### Zoxide Picker

- Type to filter
- `Enter`: create/attach session for selected directory
- `Ctrl-R`: refresh zoxide index
- `Esc`: close picker

### While Attached

- `Ctrl-G`: open picker to switch sessions
- `Ctrl-]`: detach

## Configuration

Config path:

- `$XDG_CONFIG_HOME/fish-session/config.json`
- fallback: `~/.config/fish-session/config.json`

Example:

```json
{
  "zoxide": {
    "enabled": true,
    "auto_open": false,
    "limit": 30
  }
}
```

Fields:

- `zoxide.enabled`: enable/disable zoxide mode in picker
- `zoxide.auto_open`: open picker in zoxide mode by default
- `zoxide.limit`: max displayed zoxide results

## Optional

Disable default `Ctrl-G` binding:

```fish
set -g fish_session_disable_default_bind 1
```

## Troubleshooting

If you see `fish_session: fish-session binary not found in PATH`:

1. Verify binaries are installed:

```fish
command -v fish-session
command -v fish-sessiond
```

2. If not installed, use one install path:

- Arch/AUR: `paru -S fish-session`
- Homebrew/Linuxbrew: `brew install atefr/tap/fish-session`
- Fisher + Cargo:

```fish
fisher install AtefR/fish-session
```

```bash
cargo install --git https://github.com/AtefR/fish-session.git
```

3. If using Cargo install, add Cargo bin to Fish `PATH`:

```fish
fish_add_path ~/.cargo/bin
```

4. Open a new shell and run `Ctrl-G`.
