//! Reads a child's pipe into a capped buffer, and can later hand what it reads
//! to a background job.
//!
//! A foreground command that starts `npm run dev &` leaves the dev server holding
//! the pipes the shell was given. Waiting for end-of-file (what
//! `Child::wait_with_output` does) then blocks until the command's timeout and
//! kills the server the agent just started; closing our end instead makes the
//! server die of SIGPIPE on its next log line. So the pipes are read by tasks of
//! their own: the caller waits for the *shell* to exit, gives the readers a
//! moment to see end-of-file, and when they do not, the server still holds the
//! pipe — the readers are then attached to the job and the log keeps arriving.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;

use crate::tools::bg_jobs::BgJob;

#[derive(Clone)]
pub struct Capture {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capture")
    }
}

struct Inner {
    state: Mutex<State>,
    done: watch::Receiver<bool>,
}

struct State {
    buf: Vec<u8>,
    cap: usize,
    total: usize,
    sink: Option<Arc<BgJob>>,
    /// The end of a chunk that stopped inside a UTF-8 character.
    incomplete: Vec<u8>,
}

impl Capture {
    /// Start reading `reader` (nothing to read for `None`). Keeps at most `cap`
    /// bytes; `cap = 0` keeps nothing, for a reader that only feeds a job.
    pub fn spawn<R>(reader: Option<R>, cap: usize) -> Capture
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let (tx, rx) = watch::channel(reader.is_none());
        let inner = Arc::new(Inner {
            state: Mutex::new(State { buf: Vec::new(), cap, total: 0, sink: None, incomplete: Vec::new() }),
            done: rx,
        });
        if let Some(mut r) = reader {
            let inner = inner.clone();
            tokio::spawn(async move {
                let mut chunk = vec![0u8; 8192];
                loop {
                    match r.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => inner.push(&chunk[..n]),
                    }
                }
                let _ = tx.send(true);
            });
        }
        Capture { inner }
    }

    /// A reader that only feeds `job` and keeps nothing of its own.
    pub fn feed<R>(reader: Option<R>, job: &Arc<BgJob>)
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        Capture::spawn(reader, 0).attach(job.clone());
    }

    /// True once the pipe reached end-of-file, waiting at most `grace`.
    pub async fn finished_within(&self, grace: Duration) -> bool {
        let mut rx = self.inner.done.clone();
        if *rx.borrow() {
            return true;
        }
        let finished = tokio::time::timeout(grace, rx.wait_for(|done| *done)).await.is_ok();
        finished
    }

    /// What has been read so far, and whether more arrived than `cap` allowed.
    pub fn snapshot(&self) -> (Vec<u8>, bool) {
        let st = self.inner.state.lock().unwrap();
        (st.buf.clone(), st.total > st.cap)
    }

    /// From now on, everything this pipe delivers goes into `job`'s output —
    /// what was already read first.
    pub fn attach(&self, job: Arc<BgJob>) {
        let mut st = self.inner.state.lock().unwrap();
        if !st.buf.is_empty() {
            job.append_output(&String::from_utf8_lossy(&st.buf));
        }
        st.sink = Some(job);
    }
}

impl Inner {
    fn push(&self, bytes: &[u8]) {
        let mut st = self.state.lock().unwrap();
        st.total += bytes.len();
        let room = st.cap.saturating_sub(st.buf.len());
        let keep = room.min(bytes.len());
        st.buf.extend_from_slice(&bytes[..keep]);
        if let Some(job) = st.sink.clone() {
            let mut data = std::mem::take(&mut st.incomplete);
            data.extend_from_slice(bytes);
            let (text, rest) = split_utf8(data);
            st.incomplete = rest;
            if !text.is_empty() {
                job.append_output(&text);
            }
        }
    }
}

/// The two pipes of a shell that has exited but whose pipes are still open: a
/// process it started holds them.
#[derive(Clone, Debug)]
pub struct Held {
    pub out: Capture,
    pub err: Capture,
}

/// How long the readers get to see end-of-file once the shell is gone. Output
/// the shell wrote is already in the pipe, so a pipe nobody holds closes at once.
pub const PIPE_GRACE: Duration = Duration::from_millis(250);

/// How a shell's run ended.
pub enum Waited {
    /// The shell exited. `held` is set (only with `keep_leftovers`) when
    /// something it started still has its pipes.
    Exited { status: std::process::ExitStatus, held: Option<Held> },
    TimedOut,
    Failed(String),
}

/// Wait for a shell to exit within `timeout`.
///
/// Without `keep_leftovers` this is `wait_with_output`: the run ends when the
/// pipes close, so a leftover that holds them is waited for until the timeout —
/// what scheduler scripts and the REPL rely on. With it, the run ends when the
/// *shell* does and a leftover is reported as [`Held`] instead.
pub async fn wait_shell(
    child: &mut tokio::process::Child,
    timeout: Duration,
    out: &Capture,
    err: &Capture,
    keep_leftovers: bool,
) -> Waited {
    let run = async {
        let status = child.wait().await.map_err(|e| e.to_string())?;
        let grace = if keep_leftovers { PIPE_GRACE } else { Duration::from_secs(86_400) };
        let (a, b) = tokio::join!(out.finished_within(grace), err.finished_within(grace));
        let held = (keep_leftovers && !(a && b)).then(|| Held { out: out.clone(), err: err.clone() });
        Ok::<_, String>((status, held))
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok((status, held))) => Waited::Exited { status, held },
        Ok(Err(e)) => Waited::Failed(e),
        Err(_) => Waited::TimedOut,
    }
}

/// The longest valid prefix as text, and the bytes of a character cut in two by
/// the chunk boundary (kept for the next chunk).
fn split_utf8(mut bytes: Vec<u8>) -> (String, Vec<u8>) {
    match std::str::from_utf8(&bytes) {
        Ok(_) => (String::from_utf8(bytes).unwrap_or_default(), Vec::new()),
        Err(e) if e.error_len().is_none() => {
            let rest = bytes.split_off(e.valid_up_to());
            (String::from_utf8(bytes).unwrap_or_default(), rest)
        }
        Err(_) => (String::from_utf8_lossy(&bytes).into_owned(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::bg_jobs::{BgJob, JobKind};

    #[tokio::test]
    async fn reads_to_eof_and_caps() {
        let (mut w, r) = tokio::io::duplex(64);
        let cap = Capture::spawn(Some(r), 10);
        tokio::io::AsyncWriteExt::write_all(&mut w, b"0123456789abcdef").await.unwrap();
        drop(w);
        assert!(cap.finished_within(Duration::from_secs(2)).await);
        let (buf, truncated) = cap.snapshot();
        assert_eq!(buf, b"0123456789");
        assert!(truncated);
    }

    #[tokio::test]
    async fn an_open_pipe_is_not_finished() {
        let (_w, r) = tokio::io::duplex(64);
        let cap = Capture::spawn(Some(r), 10);
        assert!(!cap.finished_within(Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn attaching_flushes_what_was_read_and_keeps_streaming() {
        let (mut w, r) = tokio::io::duplex(64);
        let cap = Capture::spawn(Some(r), 100);
        tokio::io::AsyncWriteExt::write_all(&mut w, b"early ").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let job = BgJob::new("t".into(), JobKind::Bash, "x".into());
        cap.attach(job.clone());
        tokio::io::AsyncWriteExt::write_all(&mut w, b"late").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(job.output_snapshot(), "early late");
    }

    #[tokio::test]
    async fn a_character_split_across_chunks_is_not_mangled() {
        let (mut w, r) = tokio::io::duplex(64);
        let cap = Capture::spawn(Some(r), 0);
        let job = BgJob::new("t".into(), JobKind::Bash, "x".into());
        cap.attach(job.clone());
        let bytes = "Đã chạy".as_bytes().to_vec();
        // Cut inside the two-byte "Đ".
        tokio::io::AsyncWriteExt::write_all(&mut w, &bytes[..1]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        tokio::io::AsyncWriteExt::write_all(&mut w, &bytes[1..]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(job.output_snapshot(), "Đã chạy");
    }

    #[tokio::test]
    async fn no_reader_is_already_finished() {
        let cap = Capture::spawn(None::<tokio::io::Empty>, 10);
        assert!(cap.finished_within(Duration::from_millis(1)).await);
    }
}
