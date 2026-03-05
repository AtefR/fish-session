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
    let app_config = crate::config::AppConfig::load().unwrap_or_default();
    let control_bindings = ControlKeyBindings::from_config(&app_config);

    loop {
        let (attach_cols, attach_rows) = attach_dimensions_for_status_line();
        let mut stream = connect_daemon().context("failed to connect to daemon")?;
        write_request(
            &mut stream,
            &Request::Attach {
                name: current.clone(),
                rows: attach_rows,
                cols: attach_cols,
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

        match bridge_io(
            stream,
            &current,
            should_clear_before_attach,
            !should_replay,
            control_bindings,
        )? {
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

fn attach_dimensions_for_status_line() -> (Option<u16>, Option<u16>) {
    match terminal_size() {
        Ok((cols, rows)) => {
            let safe_cols = cols.max(1);
            let safe_rows = rows.max(1);
            let session_rows = if safe_rows > 1 {
                safe_rows - 1
            } else {
                safe_rows
            };
            (Some(safe_cols), Some(session_rows))
        }
        Err(_) => (None, None),
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
    control_bindings: ControlKeyBindings,
) -> Result<BridgeOutcome> {
    let _guard = RawModeGuard::new()?;
    let _screen_restore_guard =
        ScreenRestoreGuard::new(clear_screen_on_attach, swallow_initial_enter)?;
    disable_alternate_scroll_if_needed()?;
    let mut renderer = SessionRenderer::new(session_name)?;
    // Paint initial empty frame/status immediately after entering alt-screen.
    {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        renderer.render(&mut output)?;
    }
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

            if let Some((pos, _, action)) = find_control_key(&filtered, control_bindings) {
                if pos > 0 {
                    write_stream.write_all(&filtered[..pos])?;
                    write_stream.flush()?;
                }
                if action == ControlAction::Switch {
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
    let mut query_forwarder = TerminalQueryForwarder::default();
    loop {
        let count = match read_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => break,
            Err(err) => return Err(err.into()),
        };

        let queries = query_forwarder.extract_queries(&buf[..count]);
        if !queries.is_empty() {
            output.write_all(&queries)?;
            output.flush()?;
        }
        renderer.process_output(&buf[..count], &mut output)?;
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

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ControlAction {
    Switch,
    Detach,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ControlKey {
    raw_byte: u8,
    csi_primary: u32,
    csi_shifted: Option<u32>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ControlKeyBindings {
    switch: ControlKey,
    detach: ControlKey,
}

impl ControlKeyBindings {
    fn from_config(config: &crate::config::AppConfig) -> Self {
        Self {
            switch: parse_control_key_binding(config.open_key_binding())
                .unwrap_or(ControlKey::ctrl_letter(b'g')),
            detach: parse_control_key_binding(config.detach_key_binding())
                .unwrap_or(ControlKey::ctrl_right_bracket()),
        }
    }
}

impl ControlKey {
    fn ctrl_letter(lowercase: u8) -> Self {
        Self {
            raw_byte: lowercase.saturating_sub(b'a').saturating_add(1),
            csi_primary: lowercase as u32,
            csi_shifted: Some((lowercase as char).to_ascii_uppercase() as u32),
        }
    }

    fn ctrl_right_bracket() -> Self {
        Self {
            raw_byte: 0x1d,
            csi_primary: b']' as u32,
            csi_shifted: None,
        }
    }
}

fn parse_control_key_binding(binding: &str) -> Option<ControlKey> {
    let binding = binding.trim().to_ascii_lowercase();
    if binding == "ctrl-]" {
        return Some(ControlKey::ctrl_right_bracket());
    }
    let bytes = binding.as_bytes();
    if bytes.len() == 6 && &bytes[..5] == b"ctrl-" && bytes[5].is_ascii_lowercase() {
        return Some(ControlKey::ctrl_letter(bytes[5]));
    }
    None
}

fn find_control_key(
    bytes: &[u8],
    bindings: ControlKeyBindings,
) -> Option<(usize, usize, ControlAction)> {
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == bindings.switch.raw_byte {
            return Some((index, 1, ControlAction::Switch));
        }
        if *byte == bindings.detach.raw_byte {
            return Some((index, 1, ControlAction::Detach));
        }
        if *byte == 0x1b
            && let Some((len, action)) = parse_csi_u_control(&bytes[index..], bindings)
        {
            return Some((index, len, action));
        }
    }

    None
}

fn parse_csi_u_control(
    bytes: &[u8],
    bindings: ControlKeyBindings,
) -> Option<(usize, ControlAction)> {
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

    if control_codepoint_matches_binding(codepoint, bindings.switch) {
        return Some((i + 1, ControlAction::Switch));
    }
    if control_codepoint_matches_binding(codepoint, bindings.detach) {
        return Some((i + 1, ControlAction::Detach));
    }
    None
}

fn control_codepoint_matches_binding(codepoint: u32, key: ControlKey) -> bool {
    codepoint == key.csi_primary || key.csi_shifted == Some(codepoint)
}

struct SessionRenderer {
    session_name: String,
    parser: vt100::Parser,
    composed_screen: Option<vt100::Screen>,
    cols: u16,
    rows: u16,
    content_rows: u16,
}

impl SessionRenderer {
    fn new(session_name: &str) -> Result<Self> {
        let (cols, rows, content_rows) = current_terminal_layout();
        Ok(Self {
            session_name: session_name.to_string(),
            parser: vt100::Parser::new(content_rows.max(1), cols.max(1), 8_192),
            composed_screen: None,
            cols,
            rows,
            content_rows,
        })
    }

    fn process_output(&mut self, bytes: &[u8], output: &mut impl Write) -> Result<()> {
        self.refresh_layout_if_needed();
        self.parser.process(bytes);
        self.render(output)
    }

    fn render(&mut self, output: &mut impl Write) -> Result<()> {
        self.refresh_layout_if_needed();

        let frame = self.compose_frame(true);
        let mut next = vt100::Parser::new(self.rows.max(1), self.cols.max(1), 0);
        next.process(&frame);

        let rendered = if let Some(prev) = &self.composed_screen {
            next.screen().state_diff(prev)
        } else {
            next.screen().state_formatted()
        };

        if !rendered.is_empty() {
            output.write_all(&rendered)?;
            output.flush()?;
        }
        self.composed_screen = Some(next.screen().clone());
        Ok(())
    }

    fn refresh_layout_if_needed(&mut self) {
        let (cols, rows, content_rows) = current_terminal_layout();
        if cols == self.cols && rows == self.rows && content_rows == self.content_rows {
            return;
        }

        self.cols = cols;
        self.rows = rows;
        self.content_rows = content_rows;
        self.parser
            .set_size(self.content_rows.max(1), self.cols.max(1));
        self.composed_screen = None;
    }

    fn compose_frame(&self, include_status: bool) -> Vec<u8> {
        let mut frame = Vec::new();

        // Build a deterministic frame: clear, draw session viewport, draw status.
        frame.extend_from_slice(b"\x1b[?25h\x1b[m\x1b[H\x1b[2J");
        frame.extend_from_slice(b"\x1b[1;1H");
        frame.extend_from_slice(&self.parser.screen().contents_formatted());

        if include_status && self.rows >= 2 {
            frame.extend_from_slice(format!("\x1b[{};1H\x1b[2K", self.rows).as_bytes());
            frame.extend_from_slice(b"\x1b[7m");
            let label = status_label(&self.session_name, self.cols);
            frame.extend_from_slice(label.as_bytes());
            frame.extend_from_slice(b"\x1b[m");
        }

        frame.extend_from_slice(&self.parser.screen().cursor_state_formatted());
        frame.extend_from_slice(&self.parser.screen().attributes_formatted());
        frame
    }
}

fn current_terminal_layout() -> (u16, u16, u16) {
    let (cols, rows) = terminal_size().unwrap_or((80, 24));
    let safe_cols = cols.max(1);
    let safe_rows = rows.max(1);
    let content_rows = if safe_rows > 1 {
        safe_rows - 1
    } else {
        safe_rows
    };
    (safe_cols, safe_rows, content_rows)
}

fn status_label(name: &str, cols: u16) -> String {
    if cols == 0 {
        return String::new();
    }

    let mut label = format!(" {name} ");
    if label.chars().count() > cols as usize {
        label = label.chars().take(cols as usize).collect();
    }
    label
}

#[derive(Default)]
struct TerminalQueryForwarder {
    carry: Vec<u8>,
}

impl TerminalQueryForwarder {
    fn extract_queries(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(self.carry.len() + chunk.len());
        data.extend_from_slice(&self.carry);
        data.extend_from_slice(chunk);
        self.carry.clear();

        let mut out = Vec::new();
        let mut i = 0usize;
        while i < data.len() {
            if data[i] != 0x1b {
                i += 1;
                continue;
            }

            if i + 1 >= data.len() {
                self.carry.extend_from_slice(&data[i..]);
                break;
            }

            match data[i + 1] {
                b'[' => {
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
                    if is_terminal_query_csi(params, final_byte) {
                        out.extend_from_slice(&data[i..=j]);
                    }
                    i = j + 1;
                }
                b']' => {
                    if let Some(end) = osc_end(&data, i + 2) {
                        if is_terminal_query_osc(&data[i..end]) {
                            out.extend_from_slice(&data[i..end]);
                        }
                        i = end;
                    } else {
                        self.carry.extend_from_slice(&data[i..]);
                        break;
                    }
                }
                b'P' => {
                    if let Some(end) = st_end(&data, i + 2) {
                        if is_terminal_query_dcs(&data[i..end]) {
                            out.extend_from_slice(&data[i..end]);
                        }
                        i = end;
                    } else {
                        self.carry.extend_from_slice(&data[i..]);
                        break;
                    }
                }
                b'Z' => {
                    // DECID query (legacy primary DA)
                    out.extend_from_slice(&data[i..=i + 1]);
                    i += 2;
                }
                _ => {
                    i += 1;
                }
            }
        }

        out
    }
}

fn is_terminal_query_csi(params: &[u8], final_byte: u8) -> bool {
    // Device attributes + device status reports are request/response exchanges.
    if final_byte == b'c' || final_byte == b'n' {
        return true;
    }

    // XTWINOPS queries (CSI ... t) usually contain one of these request codes.
    if final_byte == b't' {
        return params
            .split(|byte| *byte == b';')
            .any(|code| matches!(code, b"11" | b"13" | b"14" | b"18" | b"19" | b"20"));
    }

    false
}

fn is_terminal_query_dcs(sequence: &[u8]) -> bool {
    if sequence.len() < 4 {
        return false;
    }

    // Sequence includes introducer (ESC P) and ST terminator.
    let body = &sequence[2..sequence.len().saturating_sub(2)];
    body.starts_with(b"$q") || body.starts_with(b"+q")
}

fn is_terminal_query_osc(sequence: &[u8]) -> bool {
    if sequence.len() < 4 {
        return false;
    }

    // Most OSC queries are shaped like: OSC <code>;? ... BEL/ST
    sequence.windows(2).any(|window| window == b";?")
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

#[cfg(test)]
mod tests {
    use super::TerminalQueryForwarder;

    #[test]
    fn forwards_primary_da_query() {
        let mut forwarder = TerminalQueryForwarder::default();
        let bytes = b"abc\x1b[cdef";
        let queries = forwarder.extract_queries(bytes);
        assert_eq!(queries, b"\x1b[c");
    }

    #[test]
    fn forwards_query_split_across_chunks() {
        let mut forwarder = TerminalQueryForwarder::default();
        let first = forwarder.extract_queries(b"\x1b[");
        assert!(first.is_empty());
        let second = forwarder.extract_queries(b"0c");
        assert_eq!(second, b"\x1b[0c");
    }

    #[test]
    fn does_not_forward_non_query_csi_sequences() {
        let mut forwarder = TerminalQueryForwarder::default();
        let queries = forwarder.extract_queries(b"\x1b[31mhello\x1b[0m");
        assert!(queries.is_empty());
    }
}
