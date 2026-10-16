//! Unix-socket NDJSON client: one connection per request.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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
    let id = format!("space-kit:{}:{}:{method}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| format!("connect {}: {e}", path.display()))?;
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
