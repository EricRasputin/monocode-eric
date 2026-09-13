use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_STREAM_BYTES: usize = 256 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const PIPE_CLOSE_TIMEOUT: Duration = Duration::from_millis(500);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    error: Option<String>,
    truncated: bool,
}

impl Capture {
    fn push(&mut self, bytes: &[u8]) {
        let remaining = MAX_STREAM_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.truncated |= bytes.len() > remaining;
    }
}

fn drain<R: Read + Send + 'static>(
    mut reader: R,
    capture: Arc<Mutex<Capture>>,
    done: mpsc::Sender<()>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => capture
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    capture
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .error = Some(error.to_string());
                    break;
                }
            }
        }
        let _ = done.send(());
    })
}

fn wait_for_pipes(
    done: &mpsc::Receiver<()>,
    stdout_thread: thread::JoinHandle<()>,
    stderr_thread: thread::JoinHandle<()>,
) -> Result<(), String> {
    let deadline = Instant::now() + PIPE_CLOSE_TIMEOUT;
    for _ in 0..2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if done.recv_timeout(remaining).is_err() {
            // A descendant may have escaped the process tree while retaining a
            // pipe. Detach the readers rather than making this call unbounded.
            drop(stdout_thread);
            drop(stderr_thread);
            return Err("Remote Git output pipes did not close".into());
        }
    }
    stdout_thread
        .join()
        .map_err(|_| "Could not read remote Git stdout".to_string())?;
    stderr_thread
        .join()
        .map_err(|_| "Could not read remote Git stderr".to_string())?;
    Ok(())
}

fn capture_bytes(capture: &Mutex<Capture>, stream: &str) -> Result<Vec<u8>, String> {
    let capture = capture
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(error) = &capture.error {
        return Err(format!("Could not read remote Git {stream}: {error}"));
    }
    if capture.truncated {
        return Err(format!(
            "Remote Git {stream} exceeded the {} KiB output limit",
            MAX_STREAM_BYTES / 1024
        ));
    }
    Ok(capture.bytes.clone())
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    let pid = i32::try_from(pid).map_err(|_| io::Error::other("Invalid child process id"))?;
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn reap_until(child: &mut std::process::Child, deadline: Instant) -> Result<bool, String> {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(true),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => return Ok(false),
            Err(error) => return Err(format!("Could not wait for Git: {error}")),
        }
    }
}

#[cfg(unix)]
fn terminate_tree(child: &mut std::process::Child, pid: u32) -> Result<(), String> {
    let _ = signal_process_group(pid, libc::SIGTERM);
    let _ = reap_until(child, Instant::now() + Duration::from_millis(100))?;
    signal_process_group(pid, libc::SIGKILL)
        .map_err(|error| format!("Could not stop remote Git process tree: {error}"))?;
    if reap_until(child, Instant::now() + PROCESS_STOP_TIMEOUT)? {
        Ok(())
    } else {
        Err("Remote Git process tree did not stop".into())
    }
}

#[cfg(windows)]
fn terminate_tree(
    child: &mut std::process::Child,
    job: &crate::windows::ScopedJob,
) -> Result<(), String> {
    if !job
        .terminate_and_wait(PROCESS_STOP_TIMEOUT)
        .map_err(|error| format!("Could not stop remote Git process tree: {error}"))?
    {
        return Err("Remote Git process tree did not stop".into());
    }
    if reap_until(child, Instant::now() + PROCESS_STOP_TIMEOUT)? {
        Ok(())
    } else {
        Err("Remote Git process did not stop".into())
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_tree(child: &mut std::process::Child) -> Result<(), String> {
    let _ = child.kill();
    if reap_until(child, Instant::now() + PROCESS_STOP_TIMEOUT)? {
        Ok(())
    } else {
        Err("Remote Git process did not stop".into())
    }
}

/// Run a remote Git command with bounded output and a hard deadline.
///
/// The process owns a dedicated process tree. Output is drained concurrently
/// so a verbose credential helper, transport, hook, or server cannot fill a
/// pipe and masquerade as a network timeout.
pub(crate) fn output(mut command: Command, timeout: Duration) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    #[cfg(windows)]
    let (mut child, job) = crate::windows::spawn_scoped_job(&mut command)
        .map_err(|error| format!("Could not run Git: {error}"))?;
    #[cfg(not(windows))]
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not run Git: {error}"))?;

    #[cfg(unix)]
    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or("Could not open remote Git stdout")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("Could not open remote Git stderr")?;
    let stdout_capture = Arc::new(Mutex::new(Capture::default()));
    let stderr_capture = Arc::new(Mutex::new(Capture::default()));
    let (done_tx, done_rx) = mpsc::channel();
    let stdout_thread = drain(stdout, Arc::clone(&stdout_capture), done_tx.clone());
    let stderr_thread = drain(stderr, Arc::clone(&stderr_capture), done_tx);

    let deadline = Instant::now() + timeout;
    let mut wait_error = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => break None,
            Err(error) => {
                wait_error = Some(format!("Could not wait for Git: {error}"));
                break None;
            }
        }
    };

    // Git may have exited while a transport or credential helper retained its
    // pipes. Always clear the scoped tree before waiting for EOF.
    #[cfg(unix)]
    let stopped = terminate_tree(&mut child, pid);
    #[cfg(windows)]
    let stopped = terminate_tree(&mut child, &job);
    #[cfg(not(any(unix, windows)))]
    let stopped = terminate_tree(&mut child);
    stopped?;

    wait_for_pipes(&done_rx, stdout_thread, stderr_thread)?;
    if let Some(error) = wait_error {
        return Err(error);
    }

    if status.is_none() {
        return Err(format!(
            "Remote Git operation timed out after {} seconds",
            timeout.as_secs()
        ));
    }
    Ok(Output {
        status: status.expect("status checked above"),
        stdout: capture_bytes(&stdout_capture, "stdout")?,
        stderr: capture_bytes(&stderr_capture, "stderr")?,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn drains_and_bounds_noisy_stdout_while_the_process_is_running() {
        let started = Instant::now();
        let error = output(
            shell("dd if=/dev/zero bs=1048576 count=1 2>/dev/null; printf done >&2"),
            Duration::from_millis(500),
        )
        .unwrap_err();

        assert!(error.contains("output limit"), "{error}");
        assert!(!error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn inherited_pipe_writer_cannot_hold_completion_open() {
        let started = Instant::now();
        let result = output(
            shell("(trap '' TERM; sleep 30) & printf ready"),
            Duration::from_secs(1),
        )
        .unwrap();

        assert!(result.status.success());
        assert_eq!(result.stdout, b"ready");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn timeout_stops_the_entire_process_group() {
        let dir = std::env::temp_dir().join(format!(
            "monocode-remote-git-timeout-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("descendant.pid");
        let script = format!(
            "(trap '' TERM; sleep 30) & printf %s $! > '{}'; wait",
            pid_file.display()
        );
        let error = output(shell(&script), Duration::from_millis(150)).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        let descendant: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while unsafe { libc::kill(descendant, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }
        assert_eq!(unsafe { libc::kill(descendant, 0) }, -1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
