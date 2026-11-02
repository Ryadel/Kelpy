//! What an action writes, as it writes it: shared with whoever shows it while it runs, and kept when it is stopped.

use std::sync::{Arc, Mutex};

use kelpy_common::lock;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::executor::MAX_OUTPUT;

/// The longest line kept whole: a program writing no line ends is shown in pieces this long.
const MAX_LINE: usize = 8 << 10;

/// The output of an action, line by line, at most [`MAX_OUTPUT`] bytes of it.
#[derive(Debug, Clone, Default)]
pub struct Output(Arc<Mutex<Written>>);

#[derive(Debug, Default)]
struct Written {
    text: String,
    /// Bytes past the limit, read and not kept.
    dropped: usize,
    /// Changes whenever `text` or `dropped` does.
    changes: u64,
}

impl Output {
    pub fn new() -> Self {
        Self::default()
    }

    /// The output so far, without the spaces that pad tables nor blank lines around it, and a number that changes
    /// whenever the output does.
    pub fn snapshot(&self) -> (u64, String) {
        let written = lock(&self.0);
        let mut text = trimmed(&written.text);
        if written.dropped > 0 {
            text.push_str(&format!("\n[… {} more bytes not kept …]", written.dropped));
        }
        (written.changes, text)
    }

    /// Adds a line.
    pub(crate) fn line(&self, line: &str) {
        let mut written = lock(&self.0);
        if written.text.len() + line.len() < MAX_OUTPUT {
            written.text.push_str(line);
            written.text.push('\n');
        } else {
            written.dropped += line.len() + 1;
        }
        written.changes += 1;
    }

    /// Adds `text`, line by line.
    pub(crate) fn text(&self, text: &str) {
        for line in text.lines() {
            self.line(line);
        }
    }

    /// Reads `pipe` to its end, adding each line as it comes. A line rewritten with carriage returns, such as a
    /// progress bar, keeps its last state.
    pub(crate) async fn pump(&self, pipe: Option<impl AsyncRead + Unpin>) {
        let Some(mut pipe) = pipe else {
            return;
        };
        let mut pending = Vec::new();
        let mut buffer = [0; 8192];
        while let Ok(read @ 1..) = pipe.read(&mut buffer).await {
            pending.extend_from_slice(&buffer[..read]);
            while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                self.line(&shown(&line[..end]));
            }
            if pending.len() > MAX_LINE {
                self.line(&shown(&pending));
                pending.clear();
            }
        }
        if !pending.is_empty() {
            self.line(&shown(&pending));
        }
    }
}

/// A line as a terminal would show it: what follows its last carriage return.
fn shown(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let last = line
        .iter()
        .rposition(|&byte| byte == b'\r')
        .map_or(line, |at| &line[at + 1..]);
    String::from_utf8_lossy(last).into_owned()
}

/// `text` without the spaces that pad tables to the right, nor the blank lines around it.
fn trimmed(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    lines.join("\n").trim_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_come_through_as_a_terminal_shows_them() {
        let output = Output::new();
        let (_, before) = output.snapshot();
        let written: &[u8] = b"first\r\n10%\r50%\r100%\nno end";
        output.pump(Some(written)).await;
        let (changes, text) = output.snapshot();
        assert_eq!(text, "first\n100%\nno end");
        assert!(before.is_empty() && changes == 3);

        let full = Output::new();
        let line = "x".repeat(1000);
        for _ in 0..(MAX_OUTPUT / 1000 + 5) {
            full.line(&line);
        }
        let (_, text) = full.snapshot();
        assert!(text.len() < MAX_OUTPUT + 100);
        assert!(
            text.ends_with("more bytes not kept …]"),
            "{}",
            &text[text.len() - 60..]
        );
    }
}
