//! The DevTools protocol over a pipe to a Chrome of our own.
//!
//! `--remote-debugging-pipe` and not a port: Chrome then listens nowhere.
//! A debugging port on loopback is still a socket, open to anything in the
//! pod that can reach loopback, and needs a WebSocket client to speak to
//! it. The pipe is two file descriptors only this process and its child
//! hold, and the protocol on it is JSON messages ended by a zero byte.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// How long one command may go unanswered before the browser is taken for
/// hung. The prototype's figure; its sessions on ten shops never met it.
const ANSWER_WITHIN: Duration = Duration::from_secs(30);

/// Chrome reads commands from descriptor 3 and writes to 4, and a child
/// process is only handed 0, 1 and 2. The shell moves our two pipes there
/// and then becomes Chrome, so the process we hold and kill is Chrome
/// itself. Its own standard input and output go to nothing: whatever it
/// prints there must not land in the protocol.
const INTO_CHROME: &str = r#"exec "$0" "$@" 3<&0 4>&1 </dev/null >/dev/null"#;

/// Why a command got no result.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CallError {
    /// Chrome answered that it would not or could not: its own words.
    #[error("{0}")]
    Answered(String),
    /// Chrome did not answer at all: gone, or hung.
    #[error("the browser {0}")]
    Gone(&'static str),
}

/// Something Chrome said without being asked.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// The target's session it came from; `None` for the browser's own.
    pub session: Option<String>,
}

type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CallError>>>>>;

/// One Chrome and the pipe to it. Dropping it kills the Chrome.
pub struct Pipe {
    into: tokio::sync::Mutex<ChildStdin>,
    next: AtomicU64,
    waiting: Waiting,
    /// Set when the pipe has closed, so that a command sent afterwards is
    /// refused at once and not left waiting for an answer nobody will read.
    closed: Arc<AtomicBool>,
    chrome: tokio::sync::Mutex<Child>,
}

impl Pipe {
    /// Starts `chrome` with `args` and the pipe, and returns it with
    /// everything it will say unasked.
    pub fn start(chrome: &Path, args: &[String]) -> std::io::Result<(Pipe, mpsc::UnboundedReceiver<Event>)> {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(INTO_CHROME)
            .arg(chrome)
            .arg("--remote-debugging-pipe")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let into = child.stdin.take().expect("stdin is piped");
        let mut out = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let waiting: Waiting = Arc::default();
        let (events, said) = mpsc::unbounded_channel();
        let answers = waiting.clone();
        let closed: Arc<AtomicBool> = Arc::default();
        let closing = closed.clone();
        tokio::spawn(async move {
            let mut message = Vec::new();
            loop {
                message.clear();
                match out.read_until(0, &mut message).await {
                    Ok(n) if n > 0 && message.last() == Some(&0) => {}
                    // The pipe closed: Chrome is gone, and so is everyone's answer.
                    _ => break,
                }
                message.pop();
                let Ok(mut message) = serde_json::from_slice::<Value>(&message) else { continue };
                match message["id"].as_u64() {
                    Some(id) => {
                        let result = match message["error"]["message"].as_str() {
                            Some(error) => Err(CallError::Answered(error.to_string())),
                            None => Ok(message["result"].take()),
                        };
                        if let Some(asker) = answers.lock().unwrap().remove(&id) {
                            let _ = asker.send(result);
                        }
                    }
                    None => {
                        let event = Event {
                            method: message["method"].as_str().unwrap_or_default().to_string(),
                            session: message["sessionId"].as_str().map(str::to_string),
                            params: message["params"].take(),
                        };
                        if events.send(event).is_err() {
                            break;
                        }
                    }
                }
            }
            // Dropping the senders tells every caller still waiting.
            let mut answers = answers.lock().unwrap();
            closing.store(true, Ordering::Relaxed);
            answers.clear();
        });
        Ok((Pipe { into: tokio::sync::Mutex::new(into), next: AtomicU64::new(1), waiting, closed, chrome: tokio::sync::Mutex::new(child) }, said))
    }

    /// Sends one command and waits for its result.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value, CallError> {
        let answer = self.send(method, params, session).await?;
        match tokio::time::timeout(ANSWER_WITHIN, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(CallError::Gone("went away")),
            Err(_) => Err(CallError::Gone("did not answer")),
        }
    }

    /// Sends one command and returns at once with where its result will
    /// arrive. For answering Chrome from the task that listens to it, which
    /// must not stand waiting while more arrives.
    pub async fn send(&self, method: &str, params: Value, session: Option<&str>) -> Result<oneshot::Receiver<Result<Value, CallError>>, CallError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let mut bytes = serde_json::to_vec(&message).expect("a JSON value serialises");
        bytes.push(0);
        let (asker, answer) = oneshot::channel();
        {
            // Under the lock the reader takes to close, so that an asker is
            // either told by it or never put on the list.
            let mut waiting = self.waiting.lock().unwrap();
            if self.closed.load(Ordering::Relaxed) {
                return Err(CallError::Gone("went away"));
            }
            waiting.insert(id, asker);
        }
        let mut into = self.into.lock().await;
        if into.write_all(&bytes).await.is_err() || into.flush().await.is_err() {
            self.waiting.lock().unwrap().remove(&id);
            return Err(CallError::Gone("went away"));
        }
        Ok(answer)
    }

    /// Kills the Chrome and waits for it to be gone, so that its profile
    /// can be removed from under it.
    pub async fn stop(&self) {
        let _ = self.chrome.lock().await.kill().await;
    }

    /// The same from where nothing can be awaited: a page that is let go
    /// of without being closed, by a call given up halfway or a server on
    /// its way down. A killed process is gone within milliseconds, and
    /// that short wait is the price of not leaving its profile half
    /// removed, or the Chrome itself running with nobody to end it.
    pub fn stop_now(&self) {
        // Taken only here and in `stop`: if it is held, a stop is under way.
        if let Ok(mut chrome) = self.chrome.try_lock() {
            kill_and_wait(&mut chrome);
        }
    }

    /// The Chrome's process, while it runs.
    #[cfg(test)]
    pub(crate) async fn chrome_process(&self) -> Option<u32> {
        self.chrome.lock().await.id()
    }
}

fn kill_and_wait(chrome: &mut Child) {
    let _ = chrome.start_kill();
    for _ in 0..200 {
        if !matches!(chrome.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Drop for Pipe {
    /// Whoever lets go of the pipe last has let go of the Chrome, and what
    /// is dropped after it, its profile above all, finds it gone.
    fn drop(&mut self) {
        kill_and_wait(self.chrome.get_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[tokio::test]
    async fn chrome_answers_over_the_pipe_and_opens_no_port() {
        let Some(chrome) = testing::chrome_or_skip() else { return };
        let _one_of_few = testing::gate().await;
        let profile = tempfile::tempdir().unwrap();
        let args = ["--headless=new".to_string(), format!("--user-data-dir={}", profile.path().display()), "about:blank".to_string()];
        let (pipe, _said) = Pipe::start(&chrome, &args).unwrap();

        let version = pipe.call("Browser.getVersion", json!({}), None).await.unwrap();
        assert!(version["product"].as_str().is_some_and(|product| !product.is_empty()), "{version}");
        // A command Chrome does not know is answered, in its words, and not left hanging.
        let unknown = pipe.call("Browser.noSuchCommand", json!({}), None).await.unwrap_err();
        assert!(matches!(unknown, CallError::Answered(_)), "{unknown:?}");
        // With a debugging port Chrome writes the port it took into its
        // profile, which is where the prototype read it from. With the pipe
        // there is none to write.
        assert!(!profile.path().join("DevToolsActivePort").exists());

        pipe.stop().await;
        let after = pipe.call("Browser.getVersion", json!({}), None).await.unwrap_err();
        assert!(matches!(after, CallError::Gone(_)), "{after:?}");
    }

    #[tokio::test]
    async fn a_chrome_that_is_not_there_is_an_error_at_once() {
        let started = std::time::Instant::now();
        // The shell starts and finds nothing to become, so the pipe closes.
        let (pipe, _said) = Pipe::start(Path::new("/definitely/not/chrome"), &[]).unwrap();
        let err = pipe.call("Browser.getVersion", json!({}), None).await.unwrap_err();
        assert!(matches!(err, CallError::Gone(_)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "it must not wait out the 30 seconds");
    }
}
