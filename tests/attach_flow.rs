use fish_session::protocol::{Request, Response};
use nix::pty::openpty;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TestDaemon {
    child: Child,
    runtime_dir: PathBuf,
    socket_path: PathBuf,
}

impl TestDaemon {
    fn spawn() -> Self {
        let runtime_dir = unique_runtime_dir();
        fs::create_dir_all(&runtime_dir).expect("failed to create runtime dir");
        let socket_path = runtime_dir.join("fish-session").join("daemon.sock");

        let child = Command::new(env::current_exe().expect("missing current test executable path"))
            .arg("--exact")
            .arg("__fish_session_test_daemon_entry")
            .arg("--nocapture")
            .env("FISH_SESSION_TEST_DAEMON", "1")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("FISH_SESSION_SHELL", "sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn fish-sessiond");

        let daemon = Self {
            child,
            runtime_dir,
            socket_path,
        };
        daemon.wait_until_ready();
        daemon
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(response) = self.request(Request::Ping)
                && response.ok
            {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not become ready");
    }

    fn request(&self, req: Request) -> io::Result<Response> {
        let mut stream = UnixStream::connect(&self.socket_path)?;
        write_request(&mut stream, &req)?;
        let line = read_line_direct(&mut stream)?;
        let response =
            serde_json::from_str::<Response>(line.trim_end()).map_err(io::Error::other)?;
        Ok(response)
    }

    fn attach(&self, name: &str) -> io::Result<UnixStream> {
        self.attach_with_replay(name, true)
    }

    fn attach_with_replay(&self, name: &str, replay: bool) -> io::Result<UnixStream> {
        let mut stream = UnixStream::connect(&self.socket_path)?;
        write_request(
            &mut stream,
            &Request::Attach {
                name: name.to_string(),
                rows: Some(24),
                cols: Some(80),
                replay: Some(replay),
            },
        )?;
        let line = read_line_direct(&mut stream)?;
        let response =
            serde_json::from_str::<Response>(line.trim_end()).map_err(io::Error::other)?;
        if !response.ok {
            return Err(io::Error::other(format!(
                "attach failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            )));
        }
        stream.set_read_timeout(Some(Duration::from_millis(120)))?;
        Ok(stream)
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

struct UiClient {
    child: Child,
    master: OwnedFd,
}

impl UiClient {
    fn spawn(runtime_dir: &std::path::Path) -> io::Result<Self> {
        let pty = openpty(None, None).map_err(io::Error::other)?;
        let master = pty.master;
        let slave_file = unsafe { File::from_raw_fd(pty.slave.into_raw_fd()) };

        let mut command = Command::new(
            env::current_exe().map_err(|err| io::Error::new(io::ErrorKind::NotFound, err))?,
        );
        command
            .arg("--exact")
            .arg("__fish_session_test_ui_entry")
            .arg("--nocapture")
            .env("FISH_SESSION_TEST_UI", "1")
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("XDG_CONFIG_HOME", runtime_dir.join("config"))
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave_file.try_clone()?))
            .stdout(Stdio::from(slave_file.try_clone()?))
            .stderr(Stdio::from(slave_file));

        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command.spawn()?;
        Ok(Self { child, master })
    }

    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        fd_write_all(self.master.as_raw_fd(), bytes)
    }
}

impl Drop for UiClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn reattach_replays_scrollback() {
    let daemon = TestDaemon::spawn();
    let session_name = format!("replay-{}", unique_id());
    let token = format!("__FISH_SESSION_REPLAY_{}__", unique_id());

    let created = daemon
        .request(Request::Create {
            name: session_name.clone(),
            cwd: None,
            terminal_env: None,
        })
        .expect("create request failed");
    assert!(created.ok, "create failed: {:?}", created.error);

    {
        let mut first_attach = daemon.attach(&session_name).expect("first attach failed");
        writeln!(first_attach, "echo {token}").expect("failed to send command");
        first_attach.flush().expect("failed to flush");
        let output = read_until_contains(&mut first_attach, &token, Duration::from_secs(5))
            .expect("failed to read first attach output");
        assert!(
            output.contains(&token),
            "first attach output missing token: {output:?}"
        );
    }

    let mut second_attach = daemon.attach(&session_name).expect("second attach failed");
    let replay = read_until_contains(&mut second_attach, &token, Duration::from_secs(4))
        .expect("failed to read replay output");
    assert!(
        replay.contains(&token),
        "replay output missing token: {replay:?}"
    );

    let deleted = daemon
        .request(Request::Delete {
            name: session_name.clone(),
        })
        .expect("delete request failed");
    assert!(deleted.ok, "delete failed: {:?}", deleted.error);
}

#[test]
fn newly_created_session_does_not_inherit_other_session_scrollback() {
    let daemon = TestDaemon::spawn();
    let source_session = format!("source-{}", unique_id());
    let target_session = format!("target-{}", unique_id());
    let token = format!("__FISH_SESSION_ISOLATION_{}__", unique_id());
    let end_marker = format!("__FISH_SESSION_ISOLATION_END_{}__", unique_id());

    let created_source = daemon
        .request(Request::Create {
            name: source_session.clone(),
            cwd: None,
            terminal_env: None,
        })
        .expect("create request failed");
    assert!(
        created_source.ok,
        "source create failed: {:?}",
        created_source.error
    );

    {
        let mut first_attach = daemon.attach(&source_session).expect("first attach failed");
        writeln!(first_attach, "echo {token}; echo {end_marker}").expect("failed to send command");
        first_attach.flush().expect("failed to flush");
        let output = read_until_contains(&mut first_attach, &end_marker, Duration::from_secs(5))
            .expect("failed to read first attach output");
        assert!(
            output.contains(&token) && output.contains(&end_marker),
            "first attach output missing token/marker: {output:?}"
        );
    }

    let created_target = daemon
        .request(Request::Create {
            name: target_session.clone(),
            cwd: None,
            terminal_env: None,
        })
        .expect("create request failed");
    assert!(
        created_target.ok,
        "target create failed: {:?}",
        created_target.error
    );

    let mut target_attach = daemon
        .attach_with_replay(&target_session, false)
        .expect("target attach failed");
    let fresh_output =
        read_for_duration(&mut target_attach, Duration::from_millis(800)).expect("read failed");
    assert!(
        !fresh_output.contains(&token),
        "target session unexpectedly inherited source scrollback token: {fresh_output:?}"
    );

    let deleted_source = daemon
        .request(Request::Delete {
            name: source_session.clone(),
        })
        .expect("delete request failed");
    assert!(
        deleted_source.ok,
        "source delete failed: {:?}",
        deleted_source.error
    );

    let deleted_target = daemon
        .request(Request::Delete {
            name: target_session.clone(),
        })
        .expect("delete request failed");
    assert!(
        deleted_target.ok,
        "target delete failed: {:?}",
        deleted_target.error
    );
}

#[test]
fn newer_attach_supersedes_older_attach() {
    let daemon = TestDaemon::spawn();
    let session_name = format!("switch-{}", unique_id());

    let created = daemon
        .request(Request::Create {
            name: session_name.clone(),
            cwd: None,
            terminal_env: None,
        })
        .expect("create request failed");
    assert!(created.ok, "create failed: {:?}", created.error);

    let mut old_attach = daemon.attach(&session_name).expect("old attach failed");
    let mut new_attach = daemon.attach(&session_name).expect("new attach failed");

    let old_superseded = wait_for_write_disconnect(&mut old_attach, Duration::from_secs(5))
        .expect("failed while waiting for old attach supersede");
    assert!(old_superseded, "old attach was not superseded");

    writeln!(new_attach, "echo ok").expect("failed to send command over new attach");
    new_attach.flush().expect("failed to flush new attach");

    let deleted = daemon
        .request(Request::Delete {
            name: session_name.clone(),
        })
        .expect("delete request failed");
    assert!(deleted.ok, "delete failed: {:?}", deleted.error);
}

#[test]
fn ui_create_flow_auto_attaches_and_detaches() {
    let daemon = TestDaemon::spawn();
    let session_name = format!("ui-create-{}", unique_id());
    let mut ui = UiClient::spawn(&daemon.runtime_dir).expect("failed to spawn ui client");

    thread::sleep(Duration::from_millis(200));
    ui.send(b"\x0e").expect("failed to send Ctrl-N");
    ui.send(session_name.as_bytes())
        .expect("failed to send session name");
    ui.send(b"\r").expect("failed to send Enter");

    wait_until(Duration::from_secs(5), || {
        let listed = daemon.request(Request::List).ok()?;
        let sessions = listed.sessions?;
        sessions
            .iter()
            .find(|session| session.name == session_name && session.attached)
            .map(|_| ())
    })
    .expect("ui create flow did not auto-attach created session");

    ui.send(b"\x1d").expect("failed to send Ctrl-] detach");
    wait_for_child_exit(&mut ui.child, Duration::from_secs(5))
        .expect("ui process did not exit after detach");

    wait_until(Duration::from_secs(5), || {
        let listed = daemon.request(Request::List).ok()?;
        let sessions = listed.sessions?;
        sessions
            .iter()
            .find(|session| session.name == session_name && !session.attached)
            .map(|_| ())
    })
    .expect("session did not end detached after ui client exit");

    let deleted = daemon
        .request(Request::Delete {
            name: session_name.clone(),
        })
        .expect("delete request failed");
    assert!(deleted.ok, "delete failed: {:?}", deleted.error);
}

#[test]
fn duplicate_session_name_is_rejected() {
    let daemon = TestDaemon::spawn();
    let session_name = format!("dup-{}", unique_id());

    let first = daemon
        .request(Request::Create {
            name: session_name.clone(),
            cwd: None,
            terminal_env: None,
        })
        .expect("first create request failed");
    assert!(first.ok, "first create failed: {:?}", first.error);

    let second = daemon
        .request(Request::Create {
            name: session_name.clone(),
            cwd: None,
            terminal_env: None,
        })
        .expect("second create request failed");
    assert!(
        !second.ok,
        "duplicate create unexpectedly succeeded: {:?}",
        second.error
    );

    let deleted = daemon
        .request(Request::Delete {
            name: session_name.clone(),
        })
        .expect("delete request failed");
    assert!(deleted.ok, "delete failed: {:?}", deleted.error);
}

#[test]
fn create_session_tracks_requested_cwd() {
    let daemon = TestDaemon::spawn();
    let session_name = format!("cwd-{}", unique_id());
    let cwd = unique_runtime_dir().join("cwd-target");
    fs::create_dir_all(&cwd).expect("failed to create cwd target");

    let created = daemon
        .request(Request::Create {
            name: session_name.clone(),
            cwd: Some(cwd.clone()),
            terminal_env: None,
        })
        .expect("create request failed");
    assert!(created.ok, "create failed: {:?}", created.error);

    let listed = daemon.request(Request::List).expect("list request failed");
    assert!(listed.ok, "list failed: {:?}", listed.error);
    let sessions = listed.sessions.unwrap_or_default();
    let session = sessions
        .iter()
        .find(|entry| entry.name == session_name)
        .expect("created session missing from list");
    assert_eq!(
        session.cwd,
        cwd,
        "session cwd mismatch: expected {}, got {}",
        cwd.display(),
        session.cwd.display()
    );

    let deleted = daemon
        .request(Request::Delete {
            name: session_name.clone(),
        })
        .expect("delete request failed");
    assert!(deleted.ok, "delete failed: {:?}", deleted.error);
    let _ = fs::remove_dir_all(cwd.parent().expect("cwd parent should exist"));
}

fn unique_runtime_dir() -> PathBuf {
    env::temp_dir().join(format!("fish-session-it-{}", unique_id()))
}

fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn write_request(stream: &mut UnixStream, req: &Request) -> io::Result<()> {
    let payload = serde_json::to_string(req).map_err(io::Error::other)?;
    stream.write_all(payload.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_line_direct(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::with_capacity(256);
    let mut buf = [0_u8; 1];

    loop {
        let count = stream.read(&mut buf)?;
        if count == 0 {
            break;
        }
        bytes.push(buf[0]);
        if buf[0] == b'\n' {
            break;
        }
    }

    String::from_utf8(bytes).map_err(io::Error::other)
}

fn read_until_contains(
    stream: &mut UnixStream,
    needle: &str,
    timeout: Duration,
) -> io::Result<String> {
    let deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut chunk = [0_u8; 4096];

    loop {
        let haystack = String::from_utf8_lossy(&data);
        if haystack.contains(needle) {
            return Ok(haystack.to_string());
        }

        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for {needle:?}, got: {haystack:?}"),
            ));
        }

        match stream.read(&mut chunk) {
            Ok(0) => return Ok(String::from_utf8_lossy(&data).to_string()),
            Ok(n) => data.extend_from_slice(&chunk[..n]),
            Err(err)
                if err.kind() == io::ErrorKind::TimedOut
                    || err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err),
        }
    }
}

fn read_for_duration(stream: &mut UnixStream, timeout: Duration) -> io::Result<String> {
    let deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut chunk = [0_u8; 4096];

    loop {
        if Instant::now() >= deadline {
            return Ok(String::from_utf8_lossy(&data).to_string());
        }

        match stream.read(&mut chunk) {
            Ok(0) => return Ok(String::from_utf8_lossy(&data).to_string()),
            Ok(n) => data.extend_from_slice(&chunk[..n]),
            Err(err)
                if err.kind() == io::ErrorKind::TimedOut
                    || err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err),
        }
    }
}

fn wait_for_write_disconnect(stream: &mut UnixStream, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;

    loop {
        if Instant::now() >= deadline {
            return Ok(false);
        }

        match stream.write_all(b"echo superseded-check\n") {
            Ok(()) => {
                stream.flush()?;
                thread::sleep(Duration::from_millis(80));
            }
            Err(err)
                if err.kind() == io::ErrorKind::BrokenPipe
                    || err.kind() == io::ErrorKind::ConnectionAborted
                    || err.kind() == io::ErrorKind::ConnectionReset
                    || err.kind() == io::ErrorKind::NotConnected =>
            {
                return Ok(true);
            }
            Err(err) => return Err(err),
        }
    }
}

fn fd_write_all(fd: i32, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let count = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if count > 0 {
            buf = &buf[count as usize..];
            continue;
        }
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
    Ok(())
}

fn wait_until<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> io::Result<T> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(value) = check() {
            return Ok(value);
        }
        thread::sleep(Duration::from_millis(40));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "condition not met in time",
    ))
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "child did not exit in time",
    ))
}

#[test]
fn __fish_session_test_daemon_entry() {
    if env::var_os("FISH_SESSION_TEST_DAEMON").is_none() {
        return;
    }

    fish_session::daemon::run_daemon().expect("daemon test entry failed");
}

#[test]
fn __fish_session_test_ui_entry() {
    if env::var_os("FISH_SESSION_TEST_UI").is_none() {
        return;
    }

    fish_session::ui::run_ui().expect("ui test entry failed");
}
