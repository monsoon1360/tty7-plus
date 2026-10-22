use std::io::{self, Read, Write};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const REAP_POLL: Duration = Duration::from_millis(25);

pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn hide_console_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub fn output_within(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    output_impl(cmd, None, timeout)
}

/// Write the complete request on a separate thread, then close stdin to send
/// EOF, while draining both output pipes and enforcing the same deadline.
pub fn output_with_input_within(
    cmd: &mut Command,
    input: &[u8],
    timeout: Duration,
) -> io::Result<Output> {
    output_impl(cmd, Some(input), timeout)
}

fn output_impl(cmd: &mut Command, input: Option<&[u8]>, timeout: Duration) -> io::Result<Output> {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    std::thread::scope(|scope| {
        let out = scope.spawn({
            let pipe = child.stdout.take();
            move || drain(pipe)
        });
        let err = scope.spawn({
            let pipe = child.stderr.take();
            move || drain(pipe)
        });
        let writer = input.map(|input| {
            let pipe = child.stdin.take();
            scope.spawn(move || -> io::Result<()> {
                if let Some(mut pipe) = pipe {
                    pipe.write_all(input)?;
                }
                Ok(())
            })
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} did not finish within {timeout:?}",
                        cmd.get_program().to_string_lossy()
                    ),
                ));
            }
            std::thread::sleep(REAP_POLL);
        };
        if let Some(writer) = writer {
            let result = writer
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("process stdin writer panicked")));
            // Preserve the child's exit code and diagnostics if it refused the
            // request before reading stdin; BrokenPipe must not hide that.
            if status.success() {
                result?;
            }
        }
        Ok(Output {
            status,
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
        })
    })
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf);
    }
    buf
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_command_that_finishes_hands_back_its_output() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let out = output_within(&mut cmd, Duration::from_secs(10)).expect("echo runs");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    #[test]
    fn a_command_that_hangs_is_killed_and_reported_as_timed_out() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let err = output_within(&mut cmd, Duration::from_millis(200))
            .expect_err("sleep 30 must not finish");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the call should return as soon as the deadline passes, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 400000 /dev/zero | tr '\\0' 'x'"]);
        let out = output_within(&mut cmd, Duration::from_secs(20)).expect("sh runs");
        assert_eq!(out.stdout.len(), 400_000);
    }

    #[test]
    fn large_stdin_and_both_output_pipes_are_drained_through_eof() {
        let input = vec![b'x'; 800_000];
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "tee /dev/stderr; printf EOF"]);
        let out = output_with_input_within(&mut cmd, &input, Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stderr, input);
        assert_eq!(&out.stdout[..input.len()], input.as_slice());
        assert_eq!(&out.stdout[input.len()..], b"EOF");
    }

    #[test]
    fn child_refusing_stdin_keeps_its_nonzero_status_and_diagnostics() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf refusal >&2; exit 17"]);
        let out = output_with_input_within(&mut cmd, &vec![b'x'; 800_000], Duration::from_secs(5))
            .unwrap();
        assert_eq!(out.status.code(), Some(17));
        assert_eq!(out.stderr, b"refusal");
    }

    #[test]
    fn blocked_stdin_writer_is_released_and_child_reaped_on_deadline() {
        let scratch = tempfile::tempdir().unwrap();
        let pidfile = scratch.path().join("pid");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf '%s' $$ > \"$1\"; exec sleep 30", "sh"]);
        cmd.arg(&pidfile);
        let started = Instant::now();
        let error =
            output_with_input_within(&mut cmd, &vec![b'x'; 800_000], Duration::from_millis(300))
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = std::fs::read_to_string(pidfile).unwrap();
        let status = Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "timed-out child must be dead and reaped");
    }
}
