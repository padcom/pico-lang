//! Running an executable that was just written (#176).
//!
//! Linux refuses to `execve` a file that some process has open for writing (`ETXTBSY`, "Text
//! file busy"). The writer itself closes the file before it is run, but in a process with several
//! threads another thread may `fork` while the file is still open. The child inherits the
//! descriptor until its own `exec` closes it (`O_CLOEXEC`), and for that moment the new file is
//! busy. Renaming the file into place does not help: the descriptor names the same inode. Waiting
//! does: the child's `exec` comes a moment later. So a spawn that fails with `ETXTBSY` is tried
//! again after a short, growing pause.
use std::io;
use std::process::{Command, ExitStatus, Output};
use std::time::Duration;

/// How many times a busy executable is tried again, and the first pause (it doubles each time:
/// 10, 20, 40, … ms, about 2.5 s in all, much longer than a fork takes to reach its `exec`).
const RETRIES: u32 = 8;
const FIRST_PAUSE: Duration = Duration::from_millis(10);

/// Whether an error is `ETXTBSY`.
fn is_busy(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::ExecutableFileBusy
}

/// Runs `start`, and again after a pause while it fails because the executable is busy.
fn retry_busy<T>(
    mut start: impl FnMut() -> io::Result<T>,
    mut pause: impl FnMut(Duration),
) -> io::Result<T> {
    let mut wait = FIRST_PAUSE;
    for _ in 0..RETRIES {
        match start() {
            Err(e) if is_busy(&e) => {
                pause(wait);
                wait *= 2;
            }
            other => return other,
        }
    }
    start()
}

/// `cmd.output()`, tried again while the executable is busy (`ETXTBSY`).
pub fn output(cmd: &mut Command) -> io::Result<Output> {
    retry_busy(|| cmd.output(), std::thread::sleep)
}

/// [`output`] with a time limit: a program still running after `limit` is killed, and the
/// result is an error of kind `TimedOut` (the golden tests: one hung program would otherwise
/// hang the whole run).
pub fn output_within(cmd: &mut Command, limit: Duration) -> io::Result<Output> {
    use std::io::Read;
    use std::process::Stdio;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = retry_busy(|| cmd.spawn(), std::thread::sleep)?;
    let read = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = read(
        child
            .stdout
            .take()
            .map(|r| Box::new(r) as Box<dyn Read + Send>),
    );
    let err = read(
        child
            .stderr
            .take()
            .map(|r| Box::new(r) as Box<dyn Read + Send>),
    );
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("still running after {} s, killed", limit.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// `cmd.status()`, tried again while the executable is busy (`ETXTBSY`).
pub fn status(cmd: &mut Command) -> io::Result<ExitStatus> {
    retry_busy(|| cmd.status(), std::thread::sleep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy() -> io::Error {
        io::Error::from(io::ErrorKind::ExecutableFileBusy)
    }

    #[cfg(unix)]
    #[test]
    fn output_within_kills_a_program_past_its_limit() {
        let mut quick = Command::new("sh");
        quick.args(["-c", "echo hi; echo oops >&2"]);
        let out = output_within(&mut quick, Duration::from_secs(30)).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");
        let mut hang = Command::new("sleep");
        hang.arg("30");
        let started = std::time::Instant::now();
        let e = output_within(&mut hang, Duration::from_millis(200)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn tries_a_busy_executable_again_until_it_starts() {
        let mut tries = 0;
        let mut pauses = Vec::new();
        let got = retry_busy(
            || {
                tries += 1;
                if tries < 4 { Err(busy()) } else { Ok(tries) }
            },
            |d| pauses.push(d.as_millis()),
        );
        assert_eq!(got.unwrap(), 4);
        assert_eq!(pauses, vec![10, 20, 40]);
    }

    #[test]
    fn gives_up_after_the_retries_with_the_busy_error() {
        let mut tries = 0;
        let got: io::Result<()> = retry_busy(
            || {
                tries += 1;
                Err(busy())
            },
            |_| {},
        );
        assert!(is_busy(&got.unwrap_err()));
        assert_eq!(tries, RETRIES + 1);
    }

    #[test]
    fn does_not_retry_another_error() {
        let mut tries = 0;
        let got: io::Result<()> = retry_busy(
            || {
                tries += 1;
                Err(io::Error::from(io::ErrorKind::NotFound))
            },
            |_| {},
        );
        assert_eq!(got.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(tries, 1);
    }
}
