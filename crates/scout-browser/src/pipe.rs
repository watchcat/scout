//! The DevTools protocol over a pipe to a Chrome of our own.
//!
//! `--remote-debugging-pipe` and not a port: Chrome then listens nowhere.
//! A debugging port on loopback is still a socket, open to anything in the
//! pod that can reach loopback, and needs a WebSocket client to speak to
//! it. The pipe is two file descriptors only this process and its child
//! hold, and the protocol on it is JSON messages ended by a zero byte.

use rustix::process::{kill_process_group, Pid, Signal};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
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
        // The shell's `exec` looks a name with no slash in it up along
        // PATH. What is started is the program at a place, or nothing.
        if !chrome.is_absolute() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{} is not a whole path to a Chrome", chrome.display())));
        }
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(INTO_CHROME)
            .arg(chrome)
            .arg("--remote-debugging-pipe")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // A process group of its own, which its helpers are born into
            // and which `kill_all` signals as one.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let into = child.stdin.take().expect("stdin is piped");
        let out = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let waiting: Waiting = Arc::default();
        let (events, said) = mpsc::unbounded_channel();
        let closed: Arc<AtomicBool> = Arc::default();
        tokio::spawn(listen(out, MAX_MESSAGE, waiting.clone(), events, closed.clone()));
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

    /// Kills the Chrome and what it started, and waits for the Chrome to
    /// be gone, so that its profile can be removed from under it.
    pub async fn stop(&self) {
        let mut chrome = self.chrome.lock().await;
        kill_all(&chrome);
        let _ = chrome.kill().await;
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

/// The most one message from Chrome may weigh. What a session asks the page
/// for is cut to size in the page (`in_page.js`), the page's markup at two
/// megabytes being the largest of it; this is several times that, for
/// whatever Chrome says that nobody cut. A message past it is not kept.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// Reads what Chrome says until the pipe closes: answers to whoever asked,
/// the rest to `events`.
async fn listen<R: AsyncBufRead + Unpin>(mut out: R, max: usize, answers: Waiting, events: mpsc::UnboundedSender<Event>, closing: Arc<AtomicBool>) {
    // The pipe closing, or failing, ends the loop: Chrome is gone, and so
    // is everyone's answer.
    while let Ok(Some((mut message, whole))) = next_message(&mut out, max).await {
        mend_surrogates(&mut message);
        let read = if whole { serde_json::from_slice::<Value>(&message).ok() } else { None };
        let Some(mut message) = read else {
            // Not dropped in silence: a call left unanswered waits half a
            // minute and then blames the browser for hanging.
            let why = if whole { "the browser's answer could not be read" } else { "the browser's answer was too large to read" };
            match id_of(&message).and_then(|id| answers.lock().unwrap().remove(&id)) {
                Some(asker) => {
                    let _ = asker.send(Err(CallError::Answered(why.to_string())));
                }
                None => tracing::warn!(why, "something Chrome said unasked was dropped"),
            }
            continue;
        };
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
}

/// How much of a message that is too large is kept: enough to see whose
/// answer it was.
const HEAD: usize = 64;

/// The next message, up to its zero byte, and whether all of it was kept.
/// One longer than `max` is read to its end and thrown away but for its
/// first bytes, so that what follows it is still read from its start.
async fn next_message<R: AsyncBufRead + Unpin>(out: &mut R, max: usize) -> std::io::Result<Option<(Vec<u8>, bool)>> {
    let mut message = Vec::new();
    let mut whole = true;
    loop {
        let available = out.fill_buf().await?;
        if available.is_empty() {
            return Ok(None);
        }
        let (taken, ended) = match available.iter().position(|byte| *byte == 0) {
            Some(at) => (at, true),
            None => (available.len(), false),
        };
        if whole && message.len() + taken > max {
            whole = false;
            message.extend_from_slice(&available[..taken]);
            message.truncate(HEAD);
        } else if whole {
            message.extend_from_slice(&available[..taken]);
        }
        out.consume(taken + usize::from(ended));
        if ended {
            return Ok(Some((message, whole)));
        }
    }
}

/// Half a letter made a whole mark. A page's text may hold half of a
/// two-part letter (JavaScript's strings are allowed to), Chrome writes it
/// out as it is, and a message with one in it is no JSON to a strict
/// reader: the whole message would be lost for one broken letter.
///
/// Chrome writes such a half as `\uD83D`. Each one that is not followed by
/// its other half, or not preceded by its first, becomes `�`, in
/// place: the two are the same length.
fn mend_surrogates(message: &mut [u8]) {
    /// The number a `\uXXXX` at `at` writes, if one is written there.
    fn escape(message: &[u8], at: usize) -> Option<u32> {
        let written = message.get(at..at + 6)?;
        if &written[..2] != b"\\u" {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(&written[2..]).ok()?, 16).ok()
    }
    let mut at = 0;
    while at < message.len() {
        if message[at] != b'\\' {
            at += 1;
            continue;
        }
        match escape(message, at) {
            Some(first) if (0xD800..0xDC00).contains(&first) => {
                if escape(message, at + 6).is_some_and(|second| (0xDC00..0xE000).contains(&second)) {
                    at += 12;
                } else {
                    message[at + 2..at + 6].copy_from_slice(b"fffd");
                    at += 6;
                }
            }
            Some(second) if (0xDC00..0xE000).contains(&second) => {
                message[at + 2..at + 6].copy_from_slice(b"fffd");
                at += 6;
            }
            Some(_) => at += 6,
            // Any other escape is a backslash and one character: `\\`
            // among them, after which a `u` is a letter.
            None => at += 2,
        }
    }
}

/// The id of the call a message answers, read off its first bytes, for a
/// message that cannot be read whole. Chrome writes an answer's id first.
fn id_of(head: &[u8]) -> Option<u64> {
    let digits = head.strip_prefix(b"{\"id\":")?;
    let end = digits.iter().position(|byte| !byte.is_ascii_digit()).unwrap_or(digits.len());
    std::str::from_utf8(&digits[..end]).ok()?.parse().ok()
}

/// Kills `chrome` and every process it started.
///
/// Chrome is several processes, and killing the first leaves the rest to
/// notice and end themselves. Measured 2026-11-20 on Chrome 157, looking
/// every fifth of a second during two runs of this crate's tests at once:
/// 18 renderers were seen alive with their Chrome already gone, none of
/// them at the next look. A moment is all it was, but nothing here should
/// rest on what a killed program's children do next. They are born into
/// the group `Pipe::start` gives the Chrome, so the group is signalled:
/// in four runs measured the same way afterwards each of 264 Chromes led
/// a group of its own, all 2704 of their helpers were in it, and none was
/// seen to outlive its Chrome.
fn kill_all(chrome: &Child) {
    // `id` is the Chrome's number, and its group's, for as long as the
    // Chrome has not been waited for; and for that long the system gives
    // the number to nothing else. Once it has been, there is no number
    // here, so a second stop cannot signal a stranger's group.
    let group = chrome.id().and_then(|id| i32::try_from(id).ok()).and_then(Pid::from_raw);
    if let Some(group) = group {
        let _ = kill_process_group(group, Signal::KILL);
    }
}

fn kill_and_wait(chrome: &mut Child) {
    kill_all(chrome);
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

    fn mended(json: &str) -> String {
        let mut bytes = json.as_bytes().to_vec();
        mend_surrogates(&mut bytes);
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn half_a_letter_is_made_a_whole_mark_and_a_whole_letter_is_left_alone() {
        assert_eq!(mended(r#"{"t":"a\ud83d"}"#), r#"{"t":"a\ufffd"}"#);
        assert_eq!(mended(r#"{"t":"\ude00b"}"#), r#"{"t":"\ufffdb"}"#);
        assert_eq!(mended(r#"{"t":"\uD83D\uDE00 \uD83D"}"#), r#"{"t":"\uD83D\uDE00 \ufffd"}"#);
        assert_eq!(mended(r#"{"t":"\ud83d\u0041"}"#), r#"{"t":"\ufffd\u0041"}"#);
        assert_eq!(mended(r#"{"t":"\ud83d\ud83d\ude00"}"#), r#"{"t":"\ufffd\ud83d\ude00"}"#);
        for whole in [r#"{"t":"\ud83d\ude00"}"#, r#"{"t":"plain \u00e9 text"}"#, r#"{"t":""}"#] {
            assert_eq!(mended(whole), whole);
        }
        // A backslash written out and then the letters u d 8 3 d is text.
        assert_eq!(mended(r#"{"t":"\\ud83d"}"#), r#"{"t":"\\ud83d"}"#);
        assert_eq!(mended(r#"{"t":"\\\ud83d"}"#), r#"{"t":"\\\ufffd"}"#);
        // Cut off in the middle of an escape, it is left for the reader to refuse.
        assert_eq!(mended(r#"{"t":"\ud8"#), r#"{"t":"\ud8"#);
        // And what comes out is something a strict reader takes.
        let read: Value = serde_json::from_str(&mended(r#"{"t":"aaa\ud83d en de rest"}"#)).unwrap();
        assert_eq!(read["t"], "aaa\u{fffd} en de rest");
    }

    #[test]
    fn the_call_a_message_answers_is_read_off_its_first_bytes() {
        assert_eq!(id_of(br#"{"id":12,"result":{"value":"xxxxxxxx"#), Some(12));
        assert_eq!(id_of(br#"{"id":7}"#), Some(7));
        assert_eq!(id_of(br#"{"method":"Page.frameNavigated","params":{"id":3}}"#), None);
        assert_eq!(id_of(b"not a message"), None);
        assert_eq!(id_of(b""), None);
    }

    /// What `asked` calls were answered with, and what was said unasked,
    /// when Chrome says `said` and then closes the pipe. `None`: the call
    /// was left without an answer when the pipe closed.
    async fn heard(said: &[u8], asked: &[u64], max: usize) -> (Vec<Option<Result<Value, CallError>>>, Vec<Event>) {
        let waiting: Waiting = Arc::default();
        let mut answers = Vec::new();
        for id in asked {
            let (asker, answer) = oneshot::channel();
            waiting.lock().unwrap().insert(*id, asker);
            answers.push(answer);
        }
        let (events, mut unasked) = mpsc::unbounded_channel();
        let closed: Arc<AtomicBool> = Arc::default();
        listen(said, max, waiting, events, closed.clone()).await;
        assert!(closed.load(Ordering::Relaxed), "the pipe is known to be closed once it is");
        let mut said_unasked = Vec::new();
        while let Ok(event) = unasked.try_recv() {
            said_unasked.push(event);
        }
        (answers.into_iter().map(|mut answer| answer.try_recv().ok()).collect(), said_unasked)
    }

    #[tokio::test]
    async fn an_answer_goes_to_whoever_asked_and_the_rest_to_whoever_listens() {
        let said = b"{\"id\":1,\"result\":{\"value\":2}}\0{\"method\":\"Page.frameStoppedLoading\",\"params\":{\"frameId\":\"F\"},\"sessionId\":\"S\"}\0{\"id\":2,\"error\":{\"message\":\"No such thing\"}}\0";
        let (answers, unasked) = heard(said, &[1, 2, 3], MAX_MESSAGE).await;
        assert_eq!(answers[0], Some(Ok(json!({ "value": 2 }))));
        assert_eq!(answers[1], Some(Err(CallError::Answered("No such thing".to_string()))));
        assert_eq!(answers[2], None, "a call still waiting when the pipe closes is told so by its closing");
        assert_eq!((unasked.len(), unasked[0].method.as_str(), unasked[0].session.as_deref()), (1, "Page.frameStoppedLoading", Some("S")));
        assert_eq!(unasked[0].params["frameId"], "F");
    }

    #[tokio::test]
    async fn half_a_letter_in_an_answer_does_not_lose_the_answer() {
        let said = b"{\"id\":1,\"result\":{\"value\":\"aaa\\ud83d\"}}\0{\"method\":\"Page.javascriptDialogOpening\",\"params\":{\"message\":\"\\ude00\"}}\0";
        let (answers, unasked) = heard(said, &[1], MAX_MESSAGE).await;
        assert_eq!(answers[0], Some(Ok(json!({ "value": "aaa\u{fffd}" }))));
        // A message to dismiss that was dropped unread would hold the page for good.
        assert_eq!((unasked.len(), &unasked[0].params["message"]), (1, &json!("\u{fffd}")));
    }

    #[tokio::test]
    async fn an_answer_that_cannot_be_read_is_an_error_to_whoever_asked_and_not_a_silence() {
        let said = b"{\"id\":1,\"result\":{\"value\":tru}}\0{\"id\":2,\"result\":{}}\0";
        let (answers, _) = heard(said, &[1, 2], MAX_MESSAGE).await;
        assert_eq!(answers[0], Some(Err(CallError::Answered("the browser's answer could not be read".to_string()))));
        assert_eq!(answers[1], Some(Ok(json!({}))), "and the next message is read as if nothing had happened");
    }

    #[tokio::test]
    async fn an_answer_too_large_to_keep_ends_that_call_and_not_the_pipe() {
        let large = format!("{{\"id\":1,\"result\":{{\"value\":\"{}\"}}}}\0{{\"id\":2,\"result\":{{}}}}\0", "x".repeat(500));
        let (answers, _) = heard(large.as_bytes(), &[1, 2], 100).await;
        assert_eq!(answers[0], Some(Err(CallError::Answered("the browser's answer was too large to read".to_string()))));
        assert_eq!(answers[1], Some(Ok(json!({}))));
        // One byte under the limit is read.
        let fits = "{\"id\":1,\"result\":{}}\0";
        assert_eq!(heard(fits.as_bytes(), &[1], fits.len() - 1).await.0[0], Some(Ok(json!({}))));
    }

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

    /// A stand-in for a Chrome, in `dir`, that starts a process of its own
    /// as Chrome starts its helpers, and writes down which. Both end by
    /// themselves in half a minute, so a run of this that fails leaves
    /// nothing behind for long.
    fn a_program_that_starts_another(dir: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("stand-in");
        std::fs::write(&program, "#!/bin/sh\nsleep 30 &\necho $! > \"$0.started\"\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    /// The process `program` wrote down, once it has.
    async fn started_by(program: &Path) -> u32 {
        let written = program.with_extension("started");
        for _ in 0..200 {
            if let Some(process) = std::fs::read_to_string(&written).ok().and_then(|said| said.trim().parse().ok()) {
                return process;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the stand-in never said what it started");
    }

    /// Whether `process` runs. One that was killed and is only still
    /// listed, until whoever inherited it takes note, does not.
    fn runs(process: u32) -> bool {
        let listed = std::process::Command::new("ps").args(["-o", "stat=", "-p", &process.to_string()]).output().unwrap();
        let state = String::from_utf8_lossy(&listed.stdout);
        !state.trim().is_empty() && !state.trim().starts_with('Z')
    }

    /// Whether `process` is gone within two seconds.
    async fn is_gone(process: u32) -> bool {
        for _ in 0..200 {
            if !runs(process) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn what_a_chrome_started_is_stopped_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let program = a_program_that_starts_another(dir.path());
        let (pipe, _said) = Pipe::start(&program, &[]).unwrap();
        let helper = started_by(&program).await;
        assert!(runs(helper), "the stand-in's own process runs while it does");
        pipe.stop().await;
        assert!(is_gone(helper).await, "process {helper} outlived the program that started it");
    }

    #[tokio::test]
    async fn what_a_chrome_started_goes_with_a_pipe_that_is_let_go_of() {
        let dir = tempfile::tempdir().unwrap();
        let program = a_program_that_starts_another(dir.path());
        let (pipe, _said) = Pipe::start(&program, &[]).unwrap();
        let helper = started_by(&program).await;
        assert!(runs(helper), "the stand-in's own process runs while it does");
        drop(pipe);
        assert!(is_gone(helper).await, "process {helper} outlived the program that started it");
    }

    #[tokio::test]
    async fn a_chrome_given_by_name_alone_is_not_started() {
        // `exec` in a shell looks a bare name up along PATH. `true` is a
        // program on every machine, and must not be run for being asked for.
        let refused = Pipe::start(Path::new("true"), &[]).err().map(|e| e.kind());
        assert_eq!(refused, Some(std::io::ErrorKind::InvalidInput));
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
