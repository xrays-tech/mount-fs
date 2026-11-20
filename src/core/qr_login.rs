//! Aliyun QR-code login via `aliyundrive-webdav qr login`.
//!
//! The upstream command prints the QR art and progress to stdout, then a
//! `refresh_token:` marker followed by the token. We echo everything except the
//! marker/token so the secret never reaches the user's terminal.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::error::{AppError, Result};

/// Overall timeout for a QR login.
pub const QR_LOGIN_TIMEOUT: Duration = Duration::from_secs(150);

/// Parse the token from a complete `qr login` output blob.
pub fn parse_qr_login_output(output: &str) -> Option<String> {
    let mut lines = output.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "refresh_token:" {
            return lines
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_string);
        }
    }
    None
}

/// Stream `qr login` output: echo everything except the token block, then
/// return the captured token.
pub fn extract_qr_token<R: BufRead>(mut reader: R, echo: &mut dyn Write) -> Result<String> {
    let mut saw_marker = false;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Err(AppError::Credential(
                "QR login ended before a refresh token was provided".to_string(),
            ));
        }

        if !saw_marker {
            if line.trim() == "refresh_token:" {
                saw_marker = true;
                continue;
            }
            echo.write_all(line.as_bytes())?;
            echo.flush()?;
        } else {
            let token = line.trim();
            if token.is_empty() {
                continue;
            }
            return Ok(token.to_string());
        }
    }
}

/// Run `aliyundrive-webdav qr login`, showing the QR and capturing the token.
pub fn qr_login_token() -> Result<String> {
    let program = which::which("aliyundrive-webdav").map_err(|_| {
        AppError::NotFound(
            "aliyundrive-webdav not found; run: mount-fs install aliyundrive-webdav --yes"
                .to_string(),
        )
    })?;

    let mut child = Command::new(program)
        .args(["qr", "login"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|err| {
            AppError::Unsupported(format!("cannot start `aliyundrive-webdav qr login`: {err}"))
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Unsupported("cannot capture QR login stdout".to_string()))?;

    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let result = extract_qr_token(BufReader::new(stdout), &mut std::io::stdout());
        let _ = sender.send(result);
    });

    let outcome = receiver.recv_timeout(QR_LOGIN_TIMEOUT);
    // The token (or an error) is already captured; make sure the child is gone.
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    match outcome {
        Ok(Ok(token)) => Ok(token),
        Ok(Err(err)) => Err(err),
        Err(RecvTimeoutError::Timeout) => Err(AppError::Unsupported(format!(
            "timed out after {}s waiting for the QR code to be scanned",
            QR_LOGIN_TIMEOUT.as_secs()
        ))),
        Err(RecvTimeoutError::Disconnected) => Err(AppError::Unsupported(
            "QR login output reader stopped unexpectedly".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const TOKEN: &str = "QR_SENTINEL_TOKEN";
    const OUTPUT: &str = "QR-CODE-ART\nPlease scan the qrcode to login in 120 seconds\n\nrefresh_token:\n\nQR_SENTINEL_TOKEN\n";

    #[test]
    fn parses_exact_format() {
        assert_eq!(parse_qr_login_output(OUTPUT).as_deref(), Some(TOKEN));
    }

    #[test]
    fn parses_trailing_whitespace_and_crlf() {
        let output = "art\r\nrefresh_token:\r\n   \r\n  QR_SENTINEL_TOKEN  \r\n\r\n";
        assert_eq!(parse_qr_login_output(output).as_deref(), Some(TOKEN));
    }

    #[test]
    fn parser_returns_none_without_marker() {
        assert_eq!(parse_qr_login_output("just some art\n"), None);
        assert_eq!(parse_qr_login_output("refresh_token:\n"), None);
    }

    #[test]
    fn extraction_echoes_qr_but_not_token_or_marker() {
        let mut echo = Vec::new();
        let token = extract_qr_token(Cursor::new(OUTPUT), &mut echo).expect("token");
        assert_eq!(token, TOKEN);

        let echoed = String::from_utf8(echo).expect("utf8");
        assert!(echoed.contains("QR-CODE-ART"));
        assert!(!echoed.contains(TOKEN), "token was echoed: {echoed}");
        assert!(
            !echoed.contains("refresh_token:"),
            "marker was echoed: {echoed}"
        );
    }
}
