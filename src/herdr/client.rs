//! Unix-socket NDJSON client: one connection per request.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, String>;

const READ_CAPACITY: usize = 1 << 20; // snapshot lines are ~100 KB
const TIMEOUT: Duration = Duration::from_secs(5);
static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn socket_path() -> PathBuf {
    std::env::var_os("HERDR_SOCKET_PATH")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::model::home().join(".config/herdr/herdr.sock"))
}

/// Send `method` with `params`, return the `result` object or the error message.
pub fn request(method: &str, params: Value) -> Result<Value> {
    request_at(&socket_path(), method, params)
}

/// `request` against the socket at `path`.
pub fn request_at(path: &Path, method: &str, params: Value) -> Result<Value> {
    let id = format!("space-kit:{}:{}:{method}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let mut stream = UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    let line = json!({"id": id, "method": method, "params": params}).to_string();
    stream.write_all(format!("{line}\n").as_bytes()).map_err(|e| format!("{method}: write: {e}"))?;

    let mut reader = BufReader::with_capacity(READ_CAPACITY, stream);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).map_err(|e| format!("{method}: read: {e}"))?;
        if n == 0 {
            return Err(format!("{method}: connection closed before response"));
        }
        let mut msg: Value = serde_json::from_str(&buf).map_err(|e| format!("{method}: bad json: {e}"))?;
        if msg["id"] != id {
            continue;
        }
        if let Some(err) = msg.get("error") {
            let message = err["message"].as_str().map(str::to_owned).unwrap_or_else(|| err.to_string());
            return Err(format!("{method}: {message}"));
        }
        return Ok(msg["result"].take());
    }
}

/// The Herdr calls a sync makes, behind a trait so tests run without a server.
pub trait Api {
    fn call(&self, method: &str, params: Value) -> Result<Value>;
}

pub struct Socket;

impl Api for Socket {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        request(method, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    /// A one-shot Herdr: answers the next request with `reply(id)` lines, then hangs up.
    fn fake_herdr(name: &str, reply: impl Fn(&str) -> Vec<String> + Send + 'static) -> PathBuf {
        let path = std::env::temp_dir().join(format!("space-kit-sock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let id = serde_json::from_str::<Value>(&line).unwrap()["id"].as_str().unwrap().to_owned();
            let mut writer = &stream;
            for out in reply(&id) {
                writer.write_all(format!("{out}\n").as_bytes()).unwrap();
            }
        });
        path
    }

    #[test]
    fn returns_the_result_of_the_matching_reply() {
        let path = fake_herdr("ok", |id| {
            vec![
                json!({"id": "someone-else", "result": {"x": 0}}).to_string(),
                json!({"id": id, "result": {"x": 1}}).to_string(),
            ]
        });
        assert_eq!(request_at(&path, "session.snapshot", json!({})).unwrap(), json!({"x": 1}), "other ids are skipped");
    }

    #[test]
    fn reports_herdr_errors_and_early_hangups() {
        let path = fake_herdr("err", |id| vec![json!({"id": id, "error": {"message": "no such pane"}}).to_string()]);
        assert_eq!(request_at(&path, "pane.focus", json!({})), Err("pane.focus: no such pane".into()));
        let path = fake_herdr("eof", |_| Vec::new());
        assert_eq!(request_at(&path, "x", json!({})), Err("x: connection closed before response".into()));
        let path = fake_herdr("bad", |_| vec!["not json".into()]);
        assert!(request_at(&path, "x", json!({})).unwrap_err().starts_with("x: bad json"));
        assert!(
            request_at(Path::new("/nonexistent.sock"), "x", json!({}))
                .unwrap_err()
                .starts_with("connect /nonexistent.sock")
        );
    }
}
