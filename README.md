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

### 1) Install binaries

You need both binaries: `fish-session` and `fish-sessiond`.

Option A (recommended, from GitHub):

```bash
cargo install --git https://github.com/AtefR/fish-session.git
```

Option B (local clone):

```bash
cargo build --release
install -Dm755 target/release/fish-session ~/.local/bin/fish-session
install -Dm755 target/release/fish-sessiond ~/.local/bin/fish-sessiond
```

Make sure `~/.local/bin` is in your `PATH`.

### 2) Install Fish plugin files with Fisher

```fish
fisher install AtefR/fish-session
```

## Quick Start

1. Open picker: `Ctrl-G`
2. Create session: `Ctrl-N`, type name, `Enter`
3. Attach selected session: `Enter`
4. Detach active session: `Ctrl-]`

## Keybindings

### Picker (Sessions)

- `Enter`: attach selected session
- `Ctrl-N`: create session
- `Ctrl-D`: delete selected session
- `Ctrl-R`: rename selected session
- `Ctrl-O`: open zoxide mode
- `Esc`: close picker (or clear search first when search is not empty)

### Picker (Zoxide)

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

## Development

```bash
cargo fmt
cargo clippy --all-targets --all-features
cargo test
```

## Architecture

- `fish-sessiond`: daemon, socket RPC, PTY session lifecycle
- `fish-session`: UI + attach client
- Fish integration files are in:
  - `functions/fish_session.fish`
  - `conf.d/fish-session.fish`
