//! The extension's line to the app, and its log. One request per connection
//! on the socket the app creates in this extension's container (in the
//! sandbox, `NSHomeDirectory` is that container).

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::protocol::{Code, Reply, Request, SOCKET};

/// A call answered within this, or the app is taken as gone (a fetch of a
/// large file keeps the line open while it downloads).
const TIMEOUT: Duration = Duration::from_secs(600);
/// The app may be starting: connecting is tried this long.
const CONNECTING: Duration = Duration::from_secs(10);

pub fn home() -> PathBuf {
    PathBuf::from(objc2_foundation::NSHomeDirectory().to_string())
}

/// To the unified log (`process == "theca-drive"`) and to `drive.log` in the
/// container.
pub fn log(message: &str) {
    let line = format!("theca-drive[{}]: {message}", std::process::id());
    if let Ok(text) = CString::new(line.clone()) {
        // SAFETY: a constant "%s" format with one valid NUL-terminated argument.
        unsafe { libc::syslog(libc::LOG_NOTICE, c"%s".as_ptr(), text.as_ptr()) };
    }
    let _ = crate::log_file::append(&home().join("drive.log"), &line, crate::log_file::MAX);
}

fn exchange(request: &Request) -> io::Result<Reply> {
    let deadline = Instant::now() + CONNECTING;
    let stream = loop {
        match UnixStream::connect(home().join(SOCKET)) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            Err(e) => return Err(e),
        }
    };
    stream.set_read_timeout(Some(TIMEOUT))?;
    let line = serde_json::to_vec(request).map_err(io::Error::other)?;
    (&stream).write_all(&line)?;
    stream.shutdown(Shutdown::Write)?;
    let mut reply = Vec::new();
    (&stream).read_to_end(&mut reply)?;
    serde_json::from_slice(&reply).map_err(io::Error::other)
}

/// The app's answer; its errors as the app said them, or "Theca isn't
/// running" when nothing answers.
pub fn ask(request: &Request) -> Result<Reply, (Code, String)> {
    match exchange(request) {
        Ok(Reply::Error { code, message }) => {
            log(&format!("{request:?}: {code:?} {message}"));
            Err((code, message))
        }
        Ok(reply) => Ok(reply),
        Err(e) => {
            log(&format!("{request:?}: the app didn't answer: {e}"));
            Err((
                Code::ServerUnreachable,
                format!("Theca isn't running ({e})"),
            ))
        }
    }
}
