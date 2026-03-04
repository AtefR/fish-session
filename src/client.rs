use crate::protocol::{Request, Response, SessionInfo, TerminalEnv};
use anyhow::{Context, Result, anyhow, bail};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size as terminal_size};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir)
            .join("fish-session")
            .join("daemon.sock");
    }

    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/fish-session-{uid}"))
        .join("fish-session")
        .join("daemon.sock")
}

pub fn ensure_daemon() -> Result<()> {
    if ping().is_ok() {
        return Ok(());
    }

    let socket = socket_path();
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create runtime dir {}", parent.display()))?;
    }

    let stdin_null = OpenOptions::new().read(true).open("/dev/null")?;
    let stdout_null = OpenOptions::new().write(true).open("/dev/null")?;
    let stderr_null = stdout_null.try_clone()?;

    let mut command = Command::new("fish-sessiond");
    command
        .stdin(Stdio::from(stdin_null))
        .stdout(Stdio::from(stdout_null))
        .stderr(Stdio::from(stderr_null));

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn().context("failed to spawn fish-sessiond")?;

    for _ in 0..30 {
        thread::sleep(Duration::from_millis(50));
        if ping().is_ok() {
            return Ok(());
        }
    }

    bail!("fish-sessiond did not start in time")
}

pub fn ping() -> Result<()> {
    let response = request(Request::Ping)?;
    if response.ok {
        Ok(())
    } else {
        bail!(
            "daemon ping failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        )
    }
}

pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    let response = request(Request::List)?;
    if !response.ok {
        bail!(
            "list failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }

    Ok(response.sessions.unwrap_or_default())
}

pub fn create_session(name: &str, cwd: Option<PathBuf>) -> Result<()> {
    let terminal_env = capture_terminal_env();
    let response = request(Request::Create {
        name: name.to_string(),
        cwd,
        terminal_env: Some(terminal_env),
    })?;
    if response.ok {
        Ok(())
    } else {
        bail!(
            "create failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        )
    }
}

fn capture_terminal_env() -> TerminalEnv {
    TerminalEnv {
        term: env::var("TERM").ok(),
        colorterm: env::var("COLORTERM").ok(),
        term_program: env::var("TERM_PROGRAM").ok(),
        term_program_version: env::var("TERM_PROGRAM_VERSION").ok(),
        terminfo: env::var("TERMINFO").ok(),
        terminfo_dirs: env::var("TERMINFO_DIRS").ok(),
    }
}

pub fn delete_session(name: &str) -> Result<()> {
    let response = request(Request::Delete {
        name: name.to_string(),
    })?;
    if response.ok {
        Ok(())
    } else {
        bail!(
            "delete failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        )
    }
}

pub fn rename_session(from: &str, to: &str) -> Result<()> {
    let response = request(Request::Rename {
        from: from.to_string(),
        to: to.to_string(),
    })?;
    if response.ok {
        Ok(())
    } else {
        bail!(
            "rename failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        )
    }
}

pub fn attach_session(name: &str) -> Result<()> {
    attach_session_with_replay(name, true)
}

pub fn attach_session_with_replay(name: &str, replay: bool) -> Result<()> {
    let mut current = name.to_string();
    let mut should_clear_before_attach = true;
    let mut should_replay = replay;

    loop {
        let mut stream = connect_daemon().context("failed to connect to daemon")?;
        write_request(
            &mut stream,
            &Request::Attach {
                name: current.clone(),
                rows: terminal_size().ok().map(|(_, rows)| rows),
                cols: terminal_size().ok().map(|(cols, _)| cols),
                replay: Some(should_replay),
            },
        )?;

        let line = read_line_direct(&mut stream)?;
        let response: Response = serde_json::from_str(line.trim_end())
            .context("failed to parse attach response from daemon")?;
        if !response.ok {
            bail!(
                "attach failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }

        match bridge_io(stream, &current, should_clear_before_attach, !should_replay)? {
            BridgeOutcome::Detached => return Ok(()),
            BridgeOutcome::SwitchRequested => {
                if let Some(next) = crate::ui::pick_session_with_active(Some(&current))? {
                    if next.name == current {
                        // Re-select current session: reconnect with replay to restore the surface.
                        should_replay = true;
                        should_clear_before_attach = true;
                        continue;
                    }
                    current = next.name;
                    should_replay = next.replay;
                    should_clear_before_attach = true;
                } else {
                    // Esc closes picker: reconnect with replay to restore the surface.
                    should_replay = true;
                    should_clear_before_attach = true;
                    continue;
                }
            }
        }
    }
}

fn connect_daemon() -> Result<UnixStream> {
    let socket = socket_path();
    UnixStream::connect(&socket)
        .with_context(|| format!("could not connect to daemon socket {}", socket.display()))
}

fn request(req: Request) -> Result<Response> {
    let mut stream = connect_daemon().context("failed to connect to daemon")?;
    write_request(&mut stream, &req)?;
    read_response(&mut stream)
}

fn write_request(stream: &mut UnixStream, req: &Request) -> Result<()> {
    let payload = serde_json::to_string(req)?;
    stream.write_all(payload.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_response(stream: &mut UnixStream) -> Result<Response> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let bytes = reader.read_line(&mut line)?;
    if bytes == 0 {
        return Err(anyhow!("daemon closed connection unexpectedly"));
    }

    let response: Response =
        serde_json::from_str(line.trim_end()).context("failed to parse daemon response")?;
    Ok(response)
}

fn read_line_direct(stream: &mut UnixStream) -> Result<String> {
    let mut bytes = Vec::with_capacity(256);
    let mut buf = [0_u8; 1];

    loop {
        let count = stream.read(&mut buf)?;
        if count == 0 {
            if bytes.is_empty() {
                bail!("daemon closed attach connection unexpectedly");
            }
            break;
        }

        bytes.push(buf[0]);
        if buf[0] == b'\n' {
            break;
        }
    }

    let line = String::from_utf8(bytes).context("attach response is not valid UTF-8")?;
    Ok(line)
}

enum BridgeOutcome {
    Detached,
    SwitchRequested,
}

fn bridge_io(
    stream: UnixStream,
    session_name: &str,
    clear_screen_on_attach: bool,
    swallow_initial_enter: bool,
) -> Result<BridgeOutcome> {
    let _guard = RawModeGuard::new()?;
    let _screen_restore_guard =
        ScreenRestoreGuard::new(clear_screen_on_attach, swallow_initial_enter)?;
    disable_alternate_scroll_if_needed()?;
    let mut status_line = StatusLineGuard::install(session_name)?;
    // Drop any pending keypress (notably Enter from picker selection)
    // before forwarding stdin into the attached session.
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }

    let mut read_stream = stream.try_clone()?;
    let mut write_stream = stream;
    let shutdown_stream = read_stream.try_clone()?;
    let switch_requested = Arc::new(AtomicBool::new(false));
    let switch_requested_for_input = Arc::clone(&switch_requested);
    let mut input_filter = TerminalReplyFilter::default();
    let swallow_initial_enter_for_input = swallow_initial_enter;

    let input_thread = thread::spawn(move || -> Result<()> {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut buf = [0_u8; 1024];
        let mut swallow_initial_enter = swallow_initial_enter_for_input;
        let swallow_deadline = Instant::now() + Duration::from_millis(300);

        loop {
            let count = input.read(&mut buf)?;
            if count == 0 {
                let _ = write_stream.shutdown(Shutdown::Both);
                break;
            }

            let mut filtered = input_filter.filter(&buf[..count]);
            if swallow_initial_enter {
                if Instant::now() > swallow_deadline {
                    swallow_initial_enter = false;
                } else if !filtered.is_empty() {
                    strip_leading_enter_events(&mut filtered);
                    if filtered.is_empty() {
                        continue;
                    }
                    swallow_initial_enter = false;
                } else {
                    continue;
                }
            }
            if filtered.is_empty() {
                continue;
            }

            if let Some((pos, _, is_switch)) = find_control_key(&filtered) {
                if pos > 0 {
                    write_stream.write_all(&filtered[..pos])?;
                    write_stream.flush()?;
                }
                if is_switch {
                    switch_requested_for_input.store(true, Ordering::Relaxed);
                }
                let _ = write_stream.shutdown(Shutdown::Both);
                let _ = shutdown_stream.shutdown(Shutdown::Both);
                break;
            }

            write_stream.write_all(&filtered)?;
            write_stream.flush()?;
        }

        Ok(())
    });

    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut buf = [0_u8; 4096];
    let mut output_filter = OutputFilter::default();
    loop {
        let count = match read_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => break,
            Err(err) => return Err(err.into()),
        };

        let filtered = output_filter.filter(&buf[..count]);
        if !filtered.is_empty() {
            output.write_all(&filtered)?;
        }
        output.flush()?;
        status_line.redraw()?;
    }
    let pending = output_filter.finish();
    if !pending.is_empty() {
        output.write_all(&pending)?;
        output.flush()?;
    }
    let nested_alt_depth = output_filter.take_alt_screen_depth();
    if nested_alt_depth > 0 {
        for _ in 0..nested_alt_depth {
            write!(output, "\x1b[?1049l")?;
        }
        output.flush()?;
    }

    match input_thread.join() {
        Ok(result) => result?,
        Err(_) => return Err(anyhow!("input forwarding thread panicked")),
    };

    if switch_requested.load(Ordering::Relaxed) {
        Ok(BridgeOutcome::SwitchRequested)
    } else {
        Ok(BridgeOutcome::Detached)
    }
}

fn strip_leading_enter_events(bytes: &mut Vec<u8>) {
    loop {
        if bytes.starts_with(b"\r\n") {
            bytes.drain(0..2);
            continue;
        }
        if bytes.starts_with(b"\r") || bytes.starts_with(b"\n") {
            bytes.drain(0..1);
            continue;
        }
        if let Some(len) = parse_csi_u_enter(bytes) {
            bytes.drain(0..len);
            continue;
        }
        break;
    }
}

fn parse_csi_u_enter(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 6 || bytes[0] != 0x1b || bytes[1] != b'[' {
        return None;
    }

    let mut i = 2usize;
    let mut codepoint = 0u32;
    let mut saw_codepoint = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        saw_codepoint = true;
        codepoint = codepoint
            .saturating_mul(10)
            .saturating_add((bytes[i] - b'0') as u32);
        i += 1;
    }
    if !saw_codepoint || i >= bytes.len() || bytes[i] != b';' {
        return None;
    }

    i += 1;
    let mut saw_modifiers = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        saw_modifiers = true;
        i += 1;
    }
    if !saw_modifiers || i >= bytes.len() || bytes[i] != b'u' {
        return None;
    }

    if codepoint != 13 {
        return None;
    }

    Some(i + 1)
}

fn find_control_key(bytes: &[u8]) -> Option<(usize, usize, bool)> {
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == 0x07 {
            return Some((index, 1, true));
        }
        if *byte == 0x1d {
            return Some((index, 1, false));
        }
        if *byte == 0x1b
            && let Some((len, is_switch)) = parse_csi_u_control(&bytes[index..])
        {
            return Some((index, len, is_switch));
        }
    }

    None
}

fn parse_csi_u_control(bytes: &[u8]) -> Option<(usize, bool)> {
    if bytes.len() < 6 || bytes[0] != 0x1b || bytes[1] != b'[' {
        return None;
    }

    let mut i = 2usize;
    let mut codepoint = 0u32;
    let mut saw_codepoint = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        saw_codepoint = true;
        codepoint = codepoint
            .saturating_mul(10)
            .saturating_add((bytes[i] - b'0') as u32);
        i += 1;
    }
    if !saw_codepoint || i >= bytes.len() || bytes[i] != b';' {
        return None;
    }

    i += 1;
    let mut modifiers = 0u32;
    let mut saw_modifiers = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        saw_modifiers = true;
        modifiers = modifiers
            .saturating_mul(10)
            .saturating_add((bytes[i] - b'0') as u32);
        i += 1;
    }
    if !saw_modifiers || i >= bytes.len() || bytes[i] != b'u' {
        return None;
    }

    let ctrl_active = modifiers.saturating_sub(1) & 0b100 != 0;
    if !ctrl_active {
        return None;
    }

    let is_switch = matches!(codepoint, 103 | 71);
    let is_detach = codepoint == 93;
    if !is_switch && !is_detach {
        return None;
    }

    Some((i + 1, is_switch))
}

#[derive(Default)]
struct OutputFilter {
    carry: Vec<u8>,
    alt_screen_depth: usize,
}

impl OutputFilter {
    fn filter(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(self.carry.len() + chunk.len());
        data.extend_from_slice(&self.carry);
        data.extend_from_slice(chunk);
        self.carry.clear();

        let mut out = Vec::with_capacity(data.len());
        let mut i = 0usize;

        while i < data.len() {
            if data[i] != 0x1b {
                out.push(data[i]);
                i += 1;
                continue;
            }

            if i + 1 >= data.len() {
                self.carry.extend_from_slice(&data[i..]);
                break;
            }

            if data[i + 1] != b'[' {
                out.push(data[i]);
                i += 1;
                continue;
            }

            let mut j = i + 2;
            while j < data.len() && !(0x40..=0x7e).contains(&data[j]) {
                j += 1;
            }

            if j >= data.len() {
                self.carry.extend_from_slice(&data[i..]);
                break;
            }

            let params = &data[i + 2..j];
            let final_byte = data[j];
            if let Some(rewrite) =
                rewrite_private_mode_csi(params, final_byte, &mut self.alt_screen_depth)
            {
                out.extend_from_slice(&rewrite);
                i = j + 1;
                continue;
            }

            out.extend_from_slice(&data[i..=j]);
            i = j + 1;
        }

        out
    }

    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.carry)
    }

    fn take_alt_screen_depth(&mut self) -> usize {
        std::mem::take(&mut self.alt_screen_depth)
    }
}

fn rewrite_private_mode_csi(
    params: &[u8],
    final_byte: u8,
    alt_screen_depth: &mut usize,
) -> Option<Vec<u8>> {
    if final_byte != b'h' && final_byte != b'l' {
        return None;
    }
    if !params.starts_with(b"?") {
        return None;
    }

    let mut removed_any = false;
    let mut kept_modes: Vec<&[u8]> = Vec::new();

    for mode in params[1..].split(|byte| *byte == b';') {
        if mode.is_empty() {
            continue;
        }
        if mode == b"1007" && final_byte == b'h' {
            removed_any = true;
            continue;
        }
        if is_alt_screen_mode(mode) {
            if final_byte == b'h' {
                *alt_screen_depth = alt_screen_depth.saturating_add(1);
                kept_modes.push(mode);
            } else if *alt_screen_depth > 0 {
                *alt_screen_depth -= 1;
                kept_modes.push(mode);
            } else {
                // Ignore unmatched alt-screen disable so it cannot pop the
                // outer attach screen.
                removed_any = true;
            }
            continue;
        }
        kept_modes.push(mode);
    }

    if !removed_any {
        return None;
    }

    let mut out = Vec::new();
    if !kept_modes.is_empty() {
        out.extend_from_slice(b"\x1b[?");
        for (idx, mode) in kept_modes.iter().enumerate() {
            if idx > 0 {
                out.push(b';');
            }
            out.extend_from_slice(mode);
        }
        out.push(final_byte);
    }

    Some(out)
}

fn is_alt_screen_mode(mode: &[u8]) -> bool {
    mode == b"47" || mode == b"1047" || mode == b"1049"
}

#[derive(Default)]
struct TerminalReplyFilter {
    carry: Vec<u8>,
}

impl TerminalReplyFilter {
    fn filter(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(self.carry.len() + chunk.len());
        data.extend_from_slice(&self.carry);
        data.extend_from_slice(chunk);
        self.carry.clear();

        let mut out = Vec::with_capacity(data.len());
        let mut i = 0usize;
        while i < data.len() {
            if data[i] != 0x1b {
                out.push(data[i]);
                i += 1;
                continue;
            }

            if i + 1 >= data.len() {
                self.carry.extend_from_slice(&data[i..]);
                break;
            }

            match data[i + 1] {
                b']' => {
                    if let Some(end) = osc_end(&data, i + 2) {
                        i = end;
                        continue;
                    }
                    self.carry.extend_from_slice(&data[i..]);
                    break;
                }
                b'P' => {
                    if let Some(end) = st_end(&data, i + 2) {
                        i = end;
                        continue;
                    }
                    self.carry.extend_from_slice(&data[i..]);
                    break;
                }
                _ => {
                    out.push(data[i]);
                    i += 1;
                }
            }
        }

        out
    }
}

fn osc_end(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < data.len() {
        if data[i] == 0x07 {
            return Some(i + 1);
        }
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

fn st_end(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

struct StatusLineGuard {
    session_name: String,
    enabled: bool,
}

impl StatusLineGuard {
    fn install(session_name: &str) -> Result<Self> {
        let mut guard = Self {
            session_name: session_name.to_string(),
            enabled: false,
        };

        guard.enabled = guard.configure_scroll_region()?;
        if guard.enabled {
            guard.redraw()?;
        }

        Ok(guard)
    }

    fn configure_scroll_region(&self) -> Result<bool> {
        let (_, rows) = terminal_size().context("failed to get terminal size")?;
        if rows < 2 {
            return Ok(false);
        }

        let mut output = io::stdout().lock();
        write!(output, "\x1b7\x1b[1;{}r\x1b8", rows - 1)?;
        output.flush()?;
        Ok(true)
    }

    fn redraw(&mut self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let (cols, rows) = terminal_size().context("failed to get terminal size")?;
        if rows < 2 || cols == 0 {
            return Ok(());
        }

        let max_name = cols.saturating_sub(2) as usize;
        let mut name = self.session_name.clone();
        if name.chars().count() > max_name {
            name = name.chars().take(max_name).collect();
        }
        let label = format!(" {name} ");

        let mut output = io::stdout().lock();
        write!(
            output,
            "\x1b7\x1b[{};1H\x1b[2K\x1b[7m{}\x1b[0m\x1b8",
            rows, label
        )?;
        output.flush()?;
        Ok(())
    }
}

impl Drop for StatusLineGuard {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }

        let rows = match terminal_size() {
            Ok((_, rows)) => rows,
            Err(_) => return,
        };

        if rows < 1 {
            return;
        }

        let mut output = io::stdout().lock();
        let _ = write!(output, "\x1b7\x1b[r\x1b[{};1H\x1b[2K\x1b8", rows);
        let _ = output.flush();
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

struct ScreenRestoreGuard {}

impl ScreenRestoreGuard {
    fn new(clear_screen_on_attach: bool, force_reset_alt_screen: bool) -> Result<Self> {
        let mut output = io::stdout().lock();
        if clear_screen_on_attach {
            if force_reset_alt_screen {
                // New session auto-attach should start from a pristine screen.
                write!(output, "\x1b[?1049l\x1b[?1049h\x1b[2J\x1b[H")?;
            } else {
                write!(output, "\x1b[?1049h\x1b[2J\x1b[H")?;
            }
        } else {
            write!(output, "\x1b[?1049h")?;
        }
        output.flush()?;
        Ok(Self {})
    }
}

impl Drop for ScreenRestoreGuard {
    fn drop(&mut self) {
        let mut output = io::stdout().lock();
        let _ = write!(output, "\x1b[?1049l");
        let _ = output.flush();
    }
}

fn disable_alternate_scroll_if_needed() -> Result<()> {
    // Some terminals map wheel/trackpad scroll to Up/Down in alternate
    // screen; disable alternate scroll while attached.
    let mut output = io::stdout().lock();
    write!(output, "\x1b[?1007l")?;
    output.flush()?;
    Ok(())
}
