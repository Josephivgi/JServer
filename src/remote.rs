//! One long-lived SSH login per server. ssh runs a plain `sh` on the server and
//! JServer writes small read-only scripts to it; each script ends with a marker
//! line so the reply can be read back. Logins use BatchMode (never prompt) and
//! StrictHostKeyChecking=yes, so an unknown or changed host key is reported,
//! never accepted.

use crate::cmd;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const END: &str = "@@end-jserver";

/// Why a server could not be reached, in words for the server card.
#[derive(Clone, Debug)]
pub struct Failure {
    /// Machine status: timeout, hostkey, denied, refused, dns, down, lost, error.
    pub status: &'static str,
    pub label: &'static str,
    pub detail: String,
}

pub fn classify(stderr: &str, timed_out: bool) -> Failure {
    let s = stderr.to_lowercase();
    let detail = stderr.trim().lines().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" ");
    let detail: String = detail.chars().take(300).collect();
    let (status, label) = if s.contains("host key verification failed") || s.contains("remote host identification has changed") {
        ("hostkey", "Host key not trusted")
    } else if s.contains("permission denied") || s.contains("too many authentication failures") {
        ("denied", "Key rejected")
    } else if timed_out || s.contains("timed out") {
        ("timeout", "Timed out")
    } else if s.contains("connection refused") {
        ("refused", "Port closed")
    } else if s.contains("could not resolve") || s.contains("no such host") {
        ("dns", "Name not found")
    } else if s.contains("no route") || s.contains("unreachable") {
        ("down", "Unreachable")
    } else if s.contains("connection closed") || s.contains("connection reset") || s.contains("broken pipe") {
        ("lost", "Connection lost")
    } else {
        ("error", "Login failed")
    };
    Failure { status, label, detail }
}

enum Msg {
    Line(String),
    Eof,
}

pub struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Msg>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    closed: bool,
}

impl Session {
    /// Logs in and runs `setup`. Returns the session and how long the login took.
    pub fn connect(alias: &str, setup: &str) -> Result<(Session, Vec<String>, u64), Failure> {
        let t0 = Instant::now();
        let mut child = cmd::hidden("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "LogLevel=ERROR",
                alias,
                "sh",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Failure { status: "error", label: "ssh is missing", detail: format!("Could not start ssh: {e}. Install the Windows OpenSSH client.") })?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr_pipe = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Msg::Eof);
                        return;
                    }
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
                        if tx.send(Msg::Line(line)).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let err_buf = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                let mut q = err_buf.lock().unwrap();
                q.push_back(line);
                if q.len() > 20 {
                    q.pop_front();
                }
            }
        });
        let mut s = Session { child, stdin, rx, stderr, closed: false };
        let lines = s.exec(setup, Duration::from_secs(40))?;
        Ok((s, lines, t0.elapsed().as_millis() as u64))
    }

    fn failure(&mut self, timed_out: bool) -> Failure {
        self.closed = true;
        let _ = self.child.kill();
        // Give the stderr reader a moment to collect ssh's last words.
        std::thread::sleep(Duration::from_millis(150));
        let text = self.stderr.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
        let mut f = classify(&text, timed_out);
        if text.trim().is_empty() {
            f = if timed_out {
                Failure { status: "timeout", label: "Timed out", detail: "The server stopped answering.".into() }
            } else {
                Failure { status: "lost", label: "Connection lost", detail: "The SSH connection closed.".into() }
            };
        }
        f
    }

    /// Runs `script` in the remote shell and returns what it printed.
    pub fn exec(&mut self, script: &str, timeout: Duration) -> Result<Vec<String>, Failure> {
        if self.closed {
            return Err(Failure { status: "lost", label: "Connection lost", detail: "The SSH connection closed.".into() });
        }
        let payload = format!("{script}\necho {END}\n");
        if self.stdin.write_all(payload.as_bytes()).and_then(|_| self.stdin.flush()).is_err() {
            return Err(self.failure(false));
        }
        let deadline = Instant::now() + timeout;
        let mut lines = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Msg::Line(l)) if l == END => return Ok(lines),
                Ok(Msg::Line(l)) => lines.push(l),
                Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => return Err(self.failure(false)),
                Err(RecvTimeoutError::Timeout) => return Err(self.failure(true)),
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"exit\n");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn names_common_ssh_errors() {
        assert_eq!(classify("ssh: connect to host 1.2.3.4 port 22: Connection timed out", false).status, "timeout");
        assert_eq!(classify("root@x: Permission denied (publickey).", false).status, "denied");
        assert_eq!(classify("Host key verification failed.", false).status, "hostkey");
        assert_eq!(classify("ssh: connect to host x port 22: Connection refused", false).status, "refused");
        assert_eq!(classify("ssh: Could not resolve hostname nope: No such host is known.", false).status, "dns");
    }
}
