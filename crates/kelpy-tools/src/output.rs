//! What an action writes, as it writes it: shared with whoever shows it while it runs, and kept when it is stopped.
//!
//! Everything read is kept in the shared state as soon as it is read, the line not yet ended too: stopping the
//! reading, which drops it, loses nothing already written, such as a progress bar without a line end.

use std::sync::{Arc, Mutex};

use kelpy_common::lock;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::executor::MAX_OUTPUT;

/// The longest line kept whole: a program writing no line ends is shown in pieces this long.
const MAX_LINE: usize = 8 << 10;

/// Where a command writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Out = 0,
    Err = 1,
}

/// The output of an action, line by line, at most [`MAX_OUTPUT`] bytes of it.
#[derive(Debug, Clone, Default)]
pub struct Output(Arc<Mutex<Written>>);

#[derive(Debug, Default)]
struct Written {
    /// The ended lines.
    text: String,
    /// What each stream wrote after its last line end.
    open: [Vec<u8>; 2],
    /// Bytes past the limit, read and not kept.
    dropped: usize,
    /// Changes whenever anything above does.
    changes: u64,
}

impl Output {
    pub fn new() -> Self {
        Self::default()
    }

    /// The output so far, without the spaces that pad tables nor blank lines around it, and a number that changes
    /// whenever the output does. A line not yet ended is shown as it is.
    pub fn snapshot(&self) -> (u64, String) {
        let written = lock(&self.0);
        (written.changes, written.shown())
    }

    /// The output and its number, if the number is no longer `seen`: the text is put together only then.
    pub fn snapshot_since(&self, seen: u64) -> Option<(u64, String)> {
        let written = lock(&self.0);
        (written.changes != seen).then(|| (written.changes, written.shown()))
    }

    /// Adds a line.
    pub(crate) fn line(&self, line: &str) {
        lock(&self.0).add(line);
    }

    /// Adds `text`, line by line.
    pub(crate) fn text(&self, text: &str) {
        let mut written = lock(&self.0);
        for line in text.lines() {
            written.add(line);
        }
    }

    /// Reads `pipe`, the command's `stream`, to its end, adding each line as it comes. A line rewritten with
    /// carriage returns, such as a progress bar, keeps its last state.
    pub(crate) async fn pump(&self, pipe: Option<impl AsyncRead + Unpin>, stream: Stream) {
        let Some(mut pipe) = pipe else {
            return;
        };
        let mut buffer = [0; 8192];
        while let Ok(read @ 1..) = pipe.read(&mut buffer).await {
            lock(&self.0).take(stream, &buffer[..read]);
        }
        lock(&self.0).close(stream);
    }
}

impl Written {
    fn add(&mut self, line: &str) {
        if self.text.len() + line.len() < MAX_OUTPUT {
            self.text.push_str(line);
            self.text.push('\n');
        } else {
            self.dropped += line.len() + 1;
        }
        self.changes += 1;
    }

    /// Takes `bytes` read from `stream`: its ended lines are added, the rest stays open.
    fn take(&mut self, stream: Stream, bytes: &[u8]) {
        let mut open = std::mem::take(&mut self.open[stream as usize]);
        open.extend_from_slice(bytes);
        while let Some(end) = open.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = open.drain(..=end).collect();
            self.add(&shown(&line[..end]));
        }
        if open.len() > MAX_LINE {
            // Cut where a character ends: the bytes of one cut in two wait for the rest of it.
            let cut = match std::str::from_utf8(&open) {
                Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => {
                    error.valid_up_to()
                }
                _ => open.len(),
            };
            let line: Vec<u8> = open.drain(..cut).collect();
            self.add(&shown(&line));
        }
        self.open[stream as usize] = open;
        self.changes += 1;
    }

    /// The stream ended: what it left open is its last line.
    fn close(&mut self, stream: Stream) {
        let open = std::mem::take(&mut self.open[stream as usize]);
        if !open.is_empty() {
            self.add(&shown(&open));
        }
    }

    fn shown(&self) -> String {
        let mut text = self.text.clone();
        for open in self.open.iter().filter(|open| !open.is_empty()) {
            text.push_str(&shown(open));
            text.push('\n');
        }
        let mut text = trimmed(&text);
        if self.dropped > 0 {
            text.push_str(&format!("\n[… {} more bytes not kept …]", self.dropped));
        }
        text
    }
}

/// A line as a terminal would show it: what follows its last carriage return. The bytes of a character not yet
/// complete, at its end, are left out.
fn shown(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let last = line
        .iter()
        .rposition(|&byte| byte == b'\r')
        .map_or(line, |at| &line[at + 1..]);
    let complete = match std::str::from_utf8(last) {
        Err(error) if error.error_len().is_none() => &last[..error.valid_up_to()],
        _ => last,
    };
    String::from_utf8_lossy(complete).into_owned()
}

/// `text` without the spaces that pad tables to the right, nor the blank lines around it.
fn trimmed(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    lines.join("\n").trim_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn lines_come_through_as_a_terminal_shows_them() {
        let output = Output::new();
        let (_, before) = output.snapshot();
        let written: &[u8] = b"first\r\n10%\r50%\r100%\nno end";
        output.pump(Some(written), Stream::Out).await;
        let (_, text) = output.snapshot();
        assert_eq!(text, "first\n100%\nno end");
        assert!(before.is_empty());

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

    /// Waits until `output` changed from `seen`.
    async fn changed(output: &Output, seen: u64) -> (u64, String) {
        loop {
            if let Some(changed) = output.snapshot_since(seen) {
                return changed;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_line_not_ended_shows_at_once_and_stays_when_the_reading_stops() {
        let output = Output::new();
        let (mut writer, reader) = tokio::io::duplex(64);
        let pumping = tokio::spawn({
            let output = output.clone();
            async move { output.pump(Some(reader), Stream::Out).await }
        });
        writer.write_all(b"Copying 40%").await.unwrap();
        let (seen, text) = changed(&output, 0).await;
        assert_eq!(text, "Copying 40%");
        assert_eq!(output.snapshot_since(seen), None);
        // Stopped, as Stop drops the reading.
        pumping.abort();
        let _ = pumping.await;
        assert_eq!(output.snapshot().1, "Copying 40%");
    }

    #[tokio::test]
    async fn a_character_cut_between_two_reads_stays_whole() {
        let output = Output::new();
        let (mut writer, reader) = tokio::io::duplex(2 * MAX_LINE);
        let pumping = tokio::spawn({
            let output = output.clone();
            async move { output.pump(Some(reader), Stream::Out).await }
        });
        let euro = "€".as_bytes();
        let mut first = vec![b'a'; MAX_LINE - 1];
        first.extend_from_slice(&euro[..2]);
        writer.write_all(&first).await.unwrap();
        let (seen, text) = changed(&output, 0).await;
        assert_eq!(text, "a".repeat(MAX_LINE - 1));
        writer.write_all(&euro[2..]).await.unwrap();
        writer.write_all(b" left\n").await.unwrap();
        drop(writer);
        pumping.await.unwrap();
        let (_, text) = changed(&output, seen).await;
        assert_eq!(text, format!("{}\n€ left", "a".repeat(MAX_LINE - 1)));
    }
}
