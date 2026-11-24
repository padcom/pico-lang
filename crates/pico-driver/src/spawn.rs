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
