use crate::client::socket_path;
use crate::protocol::{Request, Response, SessionInfo, TerminalEnv};
use anyhow::{Context, Result, anyhow, bail};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::openpty;
use nix::sys::signal::{SigHandler, Signal, signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, execvp, fork};
use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Default)]
struct DaemonState {
    sessions: BTreeMap<String, Session>,
}

struct Session {
    name: String,
    cwd: PathBuf,
    pid: i32,
    master: OwnedFd,
    attached: bool,
    attach_id: u64,
    scrollback: VecDeque<u8>,
    alt_scrollback: VecDeque<u8>,
    capture_tail: Vec<u8>,
    alt_screen_active: bool,
}

const SCROLLBACK_MAX_BYTES: usize = 512 * 1024;

impl DaemonState {
    fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .values()
            .map(|session| SessionInfo {
                name: session.name.clone(),
                cwd: session.cwd.clone(),
                pid: session.pid,
                attached: session.attached,
            })
            .collect()
    }

    fn remove_by_pid(&mut self, pid: i32) {
        if let Some(name) = self
            .sessions
            .iter()
            .find(|(_, session)| session.pid == pid)
            .map(|(name, _)| name.clone())
        {
            self.sessions.remove(&name);
        }
    }
}

pub fn run_daemon() -> Result<()> {
    install_signal_handlers()?;

    let socket_path = socket_path();
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create runtime dir {}", parent.display()))?;
    }

    if socket_path.exists() {
        if UnixStream::connect(&socket_path).is_ok() {
            return Ok(());
        }
        fs::remove_file(&socket_path)
            .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;

    let state = Arc::new(Mutex::new(DaemonState::default()));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    if let Err(err) = handle_client(stream, state) {
                        let _ = writeln!(io::stderr(), "fish-sessiond: {err:#}");
                    }
                });
            }
            Err(err) => {
                let _ = writeln!(io::stderr(), "fish-sessiond: accept error: {err}");
            }
        }
    }

    Ok(())
}

fn install_signal_handlers() -> Result<()> {
    // The daemon must survive terminal closures and client disconnects.
    unsafe {
        signal(Signal::SIGHUP, SigHandler::SigIgn).context("failed to ignore SIGHUP")?;
        signal(Signal::SIGPIPE, SigHandler::SigIgn).context("failed to ignore SIGPIPE")?;
    }
    Ok(())
}

fn handle_client(mut stream: UnixStream, state: Arc<Mutex<DaemonState>>) -> Result<()> {
    let request = read_request(&mut stream)?;
    match request {
        Request::Ping => write_response(&mut stream, &Response::ok())?,
        Request::List => {
            let sessions = {
                let mut lock = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                reap_dead_sessions(&mut lock);
                lock.list()
            };
            write_response(&mut stream, &Response::with_sessions(sessions))?;
        }
        Request::Create {
            name,
            cwd,
            terminal_env,
        } => {
            let result = create_session(state, name, cwd, terminal_env);
            write_response(&mut stream, &result_to_response(result))?;
        }
        Request::Delete { name } => {
            let result = delete_session(state, &name);
            write_response(&mut stream, &result_to_response(result))?;
        }
        Request::Rename { from, to } => {
            let result = rename_session(state, &from, &to);
            write_response(&mut stream, &result_to_response(result))?;
        }
        Request::Attach {
            name,
            rows,
            cols,
            replay,
        } => {
            return attach_session(stream, state, &name, rows, cols, replay.unwrap_or(true));
        }
    }

    Ok(())
}

fn create_session(
    state: Arc<Mutex<DaemonState>>,
    name: String,
    cwd: Option<PathBuf>,
    terminal_env: Option<TerminalEnv>,
) -> Result<()> {
    if name.trim().is_empty() {
        bail!("session name cannot be empty");
    }

    let cwd = cwd
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));

    if !cwd.exists() {
        bail!("working directory does not exist: {}", cwd.display());
    }

    let shell = preferred_shell();

    let mut lock = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    reap_dead_sessions(&mut lock);

    if lock.sessions.contains_key(&name) {
        bail!("session already exists: {name}");
    }

    let (pid, master) = spawn_shell(&cwd, &shell, &name, terminal_env.as_ref())?;
    lock.sessions.insert(
        name.clone(),
        Session {
            name,
            cwd,
            pid,
            master,
            attached: false,
            attach_id: 0,
            scrollback: VecDeque::new(),
            alt_scrollback: VecDeque::new(),
            capture_tail: Vec::new(),
            alt_screen_active: false,
        },
    );

    Ok(())
}

fn delete_session(state: Arc<Mutex<DaemonState>>, name: &str) -> Result<()> {
    let mut lock = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    reap_dead_sessions(&mut lock);

    let session = lock
        .sessions
        .remove(name)
        .ok_or_else(|| anyhow!("session not found: {name}"))?;

    unsafe {
        libc::kill(-session.pid, libc::SIGTERM);
        libc::kill(session.pid, libc::SIGTERM);
    }

    Ok(())
}

fn rename_session(state: Arc<Mutex<DaemonState>>, from: &str, to: &str) -> Result<()> {
    if to.trim().is_empty() {
        bail!("new name cannot be empty");
    }

    let mut lock = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    reap_dead_sessions(&mut lock);

    if lock.sessions.contains_key(to) {
        bail!("session already exists: {to}");
    }

    let mut session = lock
        .sessions
        .remove(from)
        .ok_or_else(|| anyhow!("session not found: {from}"))?;
    session.name = to.to_string();
    lock.sessions.insert(to.to_string(), session);

    Ok(())
}

fn attach_session(
    mut stream: UnixStream,
    state: Arc<Mutex<DaemonState>>,
    name: &str,
    rows: Option<u16>,
    cols: Option<u16>,
    replay_requested: bool,
) -> Result<()> {
    let (pty_fd, attach_id, replay, preserve_alt_screen) = {
        let mut lock = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
        reap_dead_sessions(&mut lock);

        let session = lock
            .sessions
            .get_mut(name)
            .ok_or_else(|| anyhow!("session not found: {name}"))?;

        session.attach_id = session.attach_id.wrapping_add(1);
        let attach_id = session.attach_id;
        session.attached = true;
        if let (Some(rows), Some(cols)) = (rows, cols) {
            let _ = set_winsize(session.master.as_raw_fd(), rows, cols);
        }
        let pty_fd = dup_owned_fd(session.master.as_raw_fd())?;
        let (replay, preserve_alt_screen) = replay_payload(session, replay_requested);
        (pty_fd, attach_id, replay, preserve_alt_screen)
    };

    write_response(&mut stream, &Response::ok())?;
    if !replay.is_empty() {
        let replay = filter_replay_bytes(&replay, preserve_alt_screen);
        stream.write_all(&replay)?;
    }
    stream.flush()?;

    bridge_attach(&state, name, attach_id, pty_fd, &mut stream)?;

    let mut lock = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    if let Some(session) = lock.sessions.get_mut(name)
        && session.attach_id == attach_id
    {
        session.attached = false;
    }

    Ok(())
}

fn replay_payload(session: &Session, replay_requested: bool) -> (Vec<u8>, bool) {
    if !replay_requested {
        return (Vec::new(), false);
    }

    if session.alt_screen_active {
        return (session.alt_scrollback.iter().copied().collect(), true);
    }

    (session.scrollback.iter().copied().collect(), false)
}

fn set_winsize(fd: i32, rows: u16, cols: u16) -> Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &size) };
    if rc < 0 {
        return Err(anyhow!(
            "failed to set pty size to {}x{}: {}",
            cols,
            rows,
            io::Error::last_os_error()
        ));
    }

    Ok(())
}

fn bridge_attach(
    state: &Arc<Mutex<DaemonState>>,
    name: &str,
    attach_id: u64,
    pty_fd: OwnedFd,
    stream: &mut UnixStream,
) -> Result<()> {
    let mut socket_buf = [0_u8; 4096];
    let mut pty_buf = [0_u8; 4096];

    loop {
        if attach_was_superseded(state, name, attach_id)? {
            break;
        }

        let mut poll_fds = [
            PollFd::new(
                stream.as_fd(),
                PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL,
            ),
            PollFd::new(
                pty_fd.as_fd(),
                PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL,
            ),
        ];

        poll(&mut poll_fds, PollTimeout::from(250u16))?;

        let socket_events = poll_fds[0].revents().unwrap_or(PollFlags::empty());
        let pty_events = poll_fds[1].revents().unwrap_or(PollFlags::empty());

        if socket_events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            break;
        }

        if socket_events.contains(PollFlags::POLLIN) {
            let count = fd_read(stream.as_raw_fd(), &mut socket_buf)?;
            if count == 0 {
                break;
            }
            fd_write_all(pty_fd.as_raw_fd(), &socket_buf[..count])?;
        }

        if pty_events.contains(PollFlags::POLLIN) {
            let count = fd_read(pty_fd.as_raw_fd(), &mut pty_buf)?;
            if count == 0 {
                break;
            }

            append_scrollback(state, name, &pty_buf[..count])?;

            if let Err(err) = fd_write_all(stream.as_raw_fd(), &pty_buf[..count]) {
                if is_disconnect_error(&err) {
                    break;
                }
                return Err(err);
            }
        }

        if pty_events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            break;
        }
    }

    Ok(())
}

fn append_scrollback(state: &Arc<Mutex<DaemonState>>, name: &str, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }

    let mut lock = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    let Some(session) = lock.sessions.get_mut(name) else {
        return Ok(());
    };

    let captured = capture_scrollback_bytes(session, bytes);
    if captured.is_empty() {
        return Ok(());
    }

    append_bounded(&mut session.scrollback, &captured, SCROLLBACK_MAX_BYTES);
    Ok(())
}

fn capture_scrollback_bytes(session: &mut Session, bytes: &[u8]) -> Vec<u8> {
    const CAPTURE_TAIL_MAX_BYTES: usize = 64 * 1024;

    let mut input = Vec::with_capacity(session.capture_tail.len() + bytes.len());
    input.extend_from_slice(&session.capture_tail);
    input.extend_from_slice(bytes);
    session.capture_tail.clear();

    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;

    while i < input.len() {
        let byte = input[i];
        if byte != 0x1b {
            if session.alt_screen_active {
                append_bounded(
                    &mut session.alt_scrollback,
                    &input[i..i + 1],
                    SCROLLBACK_MAX_BYTES,
                );
            } else {
                out.push(byte);
            }
            i += 1;
            continue;
        }

        if i + 1 >= input.len() {
            session.capture_tail.extend_from_slice(&input[i..]);
            break;
        }

        match input[i + 1] {
            b'[' => {
                if let Some((end, params, final_byte)) = parse_csi(&input, i) {
                    let sequence = &input[i..end];
                    let was_alt_active = session.alt_screen_active;
                    let touched_alt = apply_alt_screen_private_mode(
                        params,
                        final_byte,
                        &mut session.alt_screen_active,
                    );
                    if touched_alt {
                        if !was_alt_active && session.alt_screen_active {
                            session.alt_scrollback.clear();
                            append_bounded(
                                &mut session.alt_scrollback,
                                sequence,
                                SCROLLBACK_MAX_BYTES,
                            );
                        } else if was_alt_active && !session.alt_screen_active {
                            session.alt_scrollback.clear();
                        } else if session.alt_screen_active {
                            append_bounded(
                                &mut session.alt_scrollback,
                                sequence,
                                SCROLLBACK_MAX_BYTES,
                            );
                        }
                    } else if session.alt_screen_active {
                        append_bounded(&mut session.alt_scrollback, sequence, SCROLLBACK_MAX_BYTES);
                    } else {
                        out.extend_from_slice(sequence);
                    }
                    i = end;
                    continue;
                }
                session.capture_tail.extend_from_slice(&input[i..]);
                break;
            }
            b']' => {
                if let Some((end, _payload)) = parse_osc(&input, i) {
                    let sequence = &input[i..end];
                    if session.alt_screen_active {
                        append_bounded(&mut session.alt_scrollback, sequence, SCROLLBACK_MAX_BYTES);
                    } else {
                        out.extend_from_slice(sequence);
                    }
                    i = end;
                    continue;
                }
                session.capture_tail.extend_from_slice(&input[i..]);
                break;
            }
            b'_' => {
                if let Some((end, _payload)) = parse_apc(&input, i) {
                    let sequence = &input[i..end];
                    if session.alt_screen_active {
                        append_bounded(&mut session.alt_scrollback, sequence, SCROLLBACK_MAX_BYTES);
                    } else {
                        out.extend_from_slice(sequence);
                    }
                    i = end;
                    continue;
                }
                session.capture_tail.extend_from_slice(&input[i..]);
                break;
            }
            _ => {
                if session.alt_screen_active {
                    append_bounded(
                        &mut session.alt_scrollback,
                        &input[i..i + 2],
                        SCROLLBACK_MAX_BYTES,
                    );
                } else {
                    out.push(input[i]);
                    out.push(input[i + 1]);
                }
                i += 2;
            }
        }
    }

    if session.capture_tail.len() > CAPTURE_TAIL_MAX_BYTES {
        let keep_from = session.capture_tail.len() - CAPTURE_TAIL_MAX_BYTES;
        session.capture_tail.drain(..keep_from);
    }

    out
}

fn append_bounded(buffer: &mut VecDeque<u8>, bytes: &[u8], max_bytes: usize) {
    if bytes.is_empty() {
        return;
    }

    if bytes.len() >= max_bytes {
        buffer.clear();
        buffer.extend(bytes[bytes.len() - max_bytes..].iter().copied());
        return;
    }

    while buffer.len() + bytes.len() > max_bytes {
        buffer.pop_front();
    }
    buffer.extend(bytes.iter().copied());
}

fn filter_replay_bytes(input: &[u8], preserve_alt_screen: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;

    while i < input.len() {
        if input[i] != 0x1b {
            out.push(input[i]);
            i += 1;
            continue;
        }

        if i + 1 >= input.len() {
            out.push(input[i]);
            break;
        }

        match input[i + 1] {
            b']' => {
                if let Some((end, payload)) = parse_osc(input, i) {
                    if !is_terminal_query_osc(payload) {
                        out.extend_from_slice(&input[i..end]);
                    }
                    i = end;
                    continue;
                }
            }
            b'[' => {
                if let Some((end, params, final_byte)) = parse_csi(input, i) {
                    if preserve_alt_screen {
                        if !is_terminal_query_csi(params, final_byte) {
                            out.extend_from_slice(&input[i..end]);
                        }
                    } else if let Some(rewritten) =
                        rewrite_replay_private_mode_csi(params, final_byte)
                    {
                        out.extend_from_slice(&rewritten);
                    } else if !is_terminal_query_csi(params, final_byte) {
                        out.extend_from_slice(&input[i..end]);
                    }
                    i = end;
                    continue;
                }
            }
            b'_' => {
                if let Some((end, payload)) = parse_apc(input, i) {
                    if !is_terminal_query_apc(payload) {
                        out.extend_from_slice(&input[i..end]);
                    }
                    i = end;
                    continue;
                }
            }
            _ => {}
        }

        out.push(input[i]);
        i += 1;
    }

    out
}

fn parse_osc(input: &[u8], start: usize) -> Option<(usize, &[u8])> {
    let mut i = start + 2;
    while i < input.len() {
        if input[i] == 0x07 {
            return Some((i + 1, &input[start + 2..i]));
        }
        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' {
            return Some((i + 2, &input[start + 2..i]));
        }
        i += 1;
    }
    None
}

fn parse_csi(input: &[u8], start: usize) -> Option<(usize, &[u8], u8)> {
    let mut i = start + 2;
    while i < input.len() {
        let byte = input[i];
        if (0x40..=0x7e).contains(&byte) {
            let params = &input[start + 2..i];
            return Some((i + 1, params, byte));
        }
        i += 1;
    }
    None
}

fn parse_apc(input: &[u8], start: usize) -> Option<(usize, &[u8])> {
    if start + 1 >= input.len() || input[start] != 0x1b || input[start + 1] != b'_' {
        return None;
    }

    let mut i = start + 2;
    while i + 1 < input.len() {
        if input[i] == 0x1b && input[i + 1] == b'\\' {
            return Some((i + 2, &input[start + 2..i]));
        }
        i += 1;
    }
    None
}

fn is_terminal_query_osc(payload: &[u8]) -> bool {
    payload.starts_with(b"10;?") || payload.starts_with(b"11;?")
}

fn is_terminal_query_csi(params: &[u8], final_byte: u8) -> bool {
    match final_byte {
        b'n' | b'c' | b'R' => true,
        b'u' => params.starts_with(b"?"),
        b'p' | b'y' => params.starts_with(b"?") && params.contains(&b'$'),
        _ => false,
    }
}

fn rewrite_replay_private_mode_csi(params: &[u8], final_byte: u8) -> Option<Vec<u8>> {
    if final_byte != b'h' && final_byte != b'l' {
        return None;
    }
    if !params.starts_with(b"?") {
        return None;
    }

    let mut changed = false;
    let mut kept_modes: Vec<&[u8]> = Vec::new();

    for mode in params[1..].split(|byte| *byte == b';') {
        if mode.is_empty() {
            continue;
        }
        if is_alt_screen_mode(mode) || (mode == b"1007" && final_byte == b'h') {
            changed = true;
            continue;
        }
        kept_modes.push(mode);
    }

    if !changed {
        return None;
    }

    if kept_modes.is_empty() {
        return Some(Vec::new());
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[?");
    for (idx, mode) in kept_modes.iter().enumerate() {
        if idx > 0 {
            out.push(b';');
        }
        out.extend_from_slice(mode);
    }
    out.push(final_byte);
    Some(out)
}

fn is_alt_screen_mode(mode: &[u8]) -> bool {
    mode == b"47" || mode == b"1047" || mode == b"1049"
}

fn apply_alt_screen_private_mode(
    params: &[u8],
    final_byte: u8,
    alt_screen_active: &mut bool,
) -> bool {
    if (final_byte != b'h' && final_byte != b'l') || !params.starts_with(b"?") {
        return false;
    }

    let mut touched_alt = false;
    for mode in params[1..].split(|byte| *byte == b';') {
        if is_alt_screen_mode(mode) {
            touched_alt = true;
            *alt_screen_active = final_byte == b'h';
        }
    }
    touched_alt
}

fn is_terminal_query_apc(payload: &[u8]) -> bool {
    payload.starts_with(b"G")
        && (payload.windows(3).any(|window| window == b"a=q")
            || payload.windows(3).any(|window| window == b"OK;")
            || payload.ends_with(b";OK"))
}

fn attach_was_superseded(
    state: &Arc<Mutex<DaemonState>>,
    name: &str,
    attach_id: u64,
) -> Result<bool> {
    let lock = state.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    let Some(session) = lock.sessions.get(name) else {
        return Ok(true);
    };

    Ok(session.attach_id != attach_id)
}

fn fd_read(fd: i32, buf: &mut [u8]) -> Result<usize> {
    loop {
        let count = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if count >= 0 {
            return Ok(count as usize);
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err.into());
    }
}

fn fd_write_all(fd: i32, mut buf: &[u8]) -> Result<()> {
    while !buf.is_empty() {
        let count = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if count > 0 {
            buf = &buf[count as usize..];
            continue;
        }

        if count == 0 {
            return Err(anyhow!("write returned 0 bytes"));
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err.into());
    }

    Ok(())
}

fn is_disconnect_error(err: &anyhow::Error) -> bool {
    if let Some(io_err) = err.downcast_ref::<io::Error>() {
        matches!(
            io_err.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::NotConnected
        )
    } else {
        false
    }
}

fn read_request(stream: &mut UnixStream) -> Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        bail!("empty request");
    }

    let request =
        serde_json::from_str::<Request>(line.trim_end()).context("failed to parse request JSON")?;
    Ok(request)
}

fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let payload = serde_json::to_string(response)?;
    stream.write_all(payload.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn result_to_response(result: Result<()>) -> Response {
    match result {
        Ok(()) => Response::ok(),
        Err(err) => Response::err(format!("{err:#}")),
    }
}

fn dup_owned_fd(fd: i32) -> Result<OwnedFd> {
    let duplicated = unsafe { libc::dup(fd) };
    if duplicated < 0 {
        return Err(anyhow!(
            "failed to dup pty fd: {}",
            io::Error::last_os_error()
        ));
    }

    let owned = unsafe { OwnedFd::from_raw_fd(duplicated) };
    Ok(owned)
}

fn preferred_shell() -> String {
    env::var("FISH_SESSION_SHELL").unwrap_or_else(|_| "fish".to_string())
}

fn spawn_shell(
    cwd: &Path,
    shell: &str,
    session_name: &str,
    terminal_env: Option<&TerminalEnv>,
) -> Result<(i32, OwnedFd)> {
    let pty = openpty(None, None)?;

    match unsafe { fork()? } {
        ForkResult::Child => {
            child_exec(
                cwd,
                shell,
                session_name,
                terminal_env,
                pty.master.as_raw_fd(),
                pty.slave.as_raw_fd(),
            );
            unreachable!();
        }
        ForkResult::Parent { child } => {
            drop(pty.slave);
            Ok((child.as_raw(), pty.master))
        }
    }
}

fn child_exec(
    cwd: &Path,
    shell: &str,
    session_name: &str,
    terminal_env: Option<&TerminalEnv>,
    master_fd: i32,
    slave_fd: i32,
) {
    unsafe {
        libc::close(master_fd);

        if libc::setsid() < 0 {
            libc::_exit(1);
        }

        let _ = libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);

        if libc::dup2(slave_fd, 0) < 0 || libc::dup2(slave_fd, 1) < 0 || libc::dup2(slave_fd, 2) < 0
        {
            libc::_exit(1);
        }

        if slave_fd > 2 {
            libc::close(slave_fd);
        }
    }

    let cwd_c = match CString::new(cwd.as_os_str().as_bytes()) {
        Ok(path) => path,
        Err(_) => unsafe {
            libc::_exit(1);
        },
    };

    unsafe {
        if libc::chdir(cwd_c.as_ptr()) < 0 {
            libc::_exit(1);
        }
    }

    set_child_env("__fish_session_name", session_name.as_bytes());
    set_child_env("__fish_session_cwd", cwd.as_os_str().as_bytes());
    apply_terminal_env(terminal_env);

    let shell_c = match CString::new(shell.as_bytes()) {
        Ok(shell) => shell,
        Err(_) => unsafe {
            libc::_exit(1);
        },
    };
    let interactive = CString::new("-i").expect("literal is valid");
    let args = [shell_c.as_c_str(), interactive.as_c_str()];

    let _ = execvp(shell_c.as_c_str(), &args);
    unsafe {
        libc::_exit(127);
    }
}

fn apply_terminal_env(terminal_env: Option<&TerminalEnv>) {
    let Some(terminal_env) = terminal_env else {
        return;
    };

    if let Some(value) = &terminal_env.term {
        set_child_env("TERM", value.as_bytes());
    } else {
        set_child_env("TERM", b"xterm-256color");
    }

    if let Some(value) = &terminal_env.colorterm {
        set_child_env("COLORTERM", value.as_bytes());
    }
    if let Some(value) = &terminal_env.term_program {
        set_child_env("TERM_PROGRAM", value.as_bytes());
    }
    if let Some(value) = &terminal_env.term_program_version {
        set_child_env("TERM_PROGRAM_VERSION", value.as_bytes());
    }
    if let Some(value) = &terminal_env.terminfo {
        set_child_env("TERMINFO", value.as_bytes());
    }
    if let Some(value) = &terminal_env.terminfo_dirs {
        set_child_env("TERMINFO_DIRS", value.as_bytes());
    }
}

fn set_child_env(name: &str, value: &[u8]) {
    let name_c = match CString::new(name) {
        Ok(value) => value,
        Err(_) => unsafe {
            libc::_exit(1);
        },
    };
    let value_c = match CString::new(value) {
        Ok(value) => value,
        Err(_) => unsafe {
            libc::_exit(1);
        },
    };

    unsafe {
        if libc::setenv(name_c.as_ptr(), value_c.as_ptr(), 1) < 0 {
            libc::_exit(1);
        }
    }
}

fn reap_dead_sessions(state: &mut DaemonState) {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => break,
            Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => {
                state.remove_by_pid(pid.as_raw());
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Session, apply_alt_screen_private_mode, capture_scrollback_bytes, filter_replay_bytes,
        replay_payload,
    };
    use nix::pty::openpty;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    fn test_session() -> Session {
        let pty = openpty(None, None).expect("failed to open pty");
        Session {
            name: "test".to_string(),
            cwd: PathBuf::from("/tmp"),
            pid: 1,
            master: pty.master,
            attached: false,
            attach_id: 0,
            scrollback: VecDeque::new(),
            alt_scrollback: VecDeque::new(),
            capture_tail: Vec::new(),
            alt_screen_active: false,
        }
    }

    #[test]
    fn replay_strips_alt_screen_sequences() {
        let input = b"pre\x1b[?1049hmid\x1b[?1049lpost";
        let output = filter_replay_bytes(input, false);
        let text = String::from_utf8_lossy(&output);
        assert_eq!(text, "premidpost");
    }

    #[test]
    fn replay_rewrites_mixed_private_modes() {
        let input = b"\x1b[?1049;25h";
        let output = filter_replay_bytes(input, false);
        assert_eq!(output, b"\x1b[?25h");
    }

    #[test]
    fn replay_drops_alt_scroll_enable() {
        let input = b"A\x1b[?1007hB";
        let output = filter_replay_bytes(input, false);
        assert_eq!(output, b"AB");
    }

    #[test]
    fn capture_drops_alt_screen_content_and_keeps_primary_text() {
        let mut session = test_session();
        let input = b"before\x1b[?1049hALT-SCREEN\x1b[?1049lafter";
        let output = capture_scrollback_bytes(&mut session, input);
        assert_eq!(output, b"beforeafter");
        assert!(!session.alt_screen_active);
    }

    #[test]
    fn capture_handles_split_alt_screen_sequences_across_chunks() {
        let mut session = test_session();
        let first = capture_scrollback_bytes(&mut session, b"pre\x1b[?1049");
        assert_eq!(first, b"pre");
        assert!(!session.alt_screen_active);

        let second = capture_scrollback_bytes(&mut session, b"hALT\x1b[?1049");
        assert_eq!(second, b"");
        assert!(session.alt_screen_active);

        let third = capture_scrollback_bytes(&mut session, b"lpost");
        assert_eq!(third, b"post");
        assert!(!session.alt_screen_active);
    }

    #[test]
    fn apply_alt_mode_ignores_non_alt_private_modes() {
        let mut alt_active = false;
        let touched = apply_alt_screen_private_mode(b"?25", b'h', &mut alt_active);
        assert!(!touched);
        assert!(!alt_active);
    }

    #[test]
    fn capture_keeps_alt_scrollback_while_alt_screen_is_active() {
        let mut session = test_session();
        let _ = capture_scrollback_bytes(&mut session, b"\x1b[?1049hALT");
        assert!(session.alt_screen_active);
        let captured: Vec<u8> = session.alt_scrollback.iter().copied().collect();
        assert_eq!(captured, b"\x1b[?1049hALT");
    }

    #[test]
    fn replay_payload_prefers_alt_scrollback_when_active() {
        let mut session = test_session();
        session.scrollback.extend(b"PRIMARY".iter().copied());
        session.alt_scrollback.extend(b"ALT".iter().copied());
        session.alt_screen_active = true;

        let (payload, preserve_alt_screen) = replay_payload(&session, true);
        assert_eq!(payload, b"ALT");
        assert!(preserve_alt_screen);
    }
}
