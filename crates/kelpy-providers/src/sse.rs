//! A Server-Sent Events parser for provider responses: bytes in, complete events out.
//!
//! Chunks may split lines and even UTF-8 characters anywhere, so bytes are buffered until a full line arrives. An
//! event may hold at most [`MAX_EVENT`] bytes, so a stream that never ends a line or an event cannot fill memory.

use crate::ProviderError;

/// The most an event may hold. A Responses stream's last event repeats the whole answer, itself limited by Kelpy.
pub const MAX_EVENT: usize = 4 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field, if any.
    pub name: Option<String>,
    /// The `data:` lines, joined with newlines.
    pub data: String,
}

#[derive(Debug)]
pub struct Parser {
    buffer: Vec<u8>,
    name: Option<String>,
    data: Vec<String>,
    /// Bytes in `data`.
    size: usize,
    limit: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Self::with_limit(MAX_EVENT)
    }
}

impl Parser {
    /// A parser of events of at most `limit` bytes.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buffer: Vec::new(),
            name: None,
            data: Vec::new(),
            size: 0,
            limit,
        }
    }

    /// Feeds a chunk and returns the events it completed; fails once an event passes the limit.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Event>, ProviderError> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if let Some(event) = self.dispatch() {
                    events.push(event);
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((&line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.name = Some(value.to_owned()),
                "data" => {
                    // Checked before the line is kept, so an event's size never depends on how it was cut.
                    self.size += value.len();
                    self.check(0)?;
                    self.data.push(value.to_owned());
                }
                _ => {}
            }
        }
        // A line not ended yet.
        self.check(self.buffer.len())?;
        Ok(events)
    }

    /// Fails if the event under way, with `pending` bytes more, passes the limit.
    fn check(&self, pending: usize) -> Result<(), ProviderError> {
        if self.size + pending > self.limit {
            return Err(ProviderError::TooLarge(format!(
                "an event of the stream is larger than {} bytes",
                self.limit
            )));
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Option<Event> {
        let name = self.name.take();
        self.size = 0;
        if self.data.is_empty() {
            return None;
        }
        Some(Event {
            name,
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks_are_reassembled() {
        let mut parser = Parser::default();
        let stream =
            "event: delta\r\ndata: {\"text\":\"caff\u{e8}\"}\r\n\r\n: ping\n\ndata: [DONE]\n\n";
        let bytes = stream.as_bytes();
        let mut events = Vec::new();
        // One byte at a time: splits lines, CRLF pairs and the two bytes of "è".
        for byte in bytes {
            events.extend(parser.push(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(
            events,
            vec![
                Event {
                    name: Some("delta".into()),
                    data: "{\"text\":\"caff\u{e8}\"}".into()
                },
                Event {
                    name: None,
                    data: "[DONE]".into()
                },
            ]
        );
    }

    #[test]
    fn multi_line_data_is_joined() {
        let mut parser = Parser::default();
        let events = parser.push(b"data: a\ndata: b\n\n").unwrap();
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn an_event_larger_than_the_limit_fails() {
        // A line never ended.
        let mut parser = Parser::with_limit(16);
        assert!(parser.push(b"data: 0123456789").is_ok());
        assert!(parser.push(b"abc").is_err());
        // Short lines, never an end of event.
        let mut parser = Parser::with_limit(16);
        assert!(parser.push(b"data: 01234567\n").is_ok());
        assert!(parser.push(b"data: 89abcdef\n").is_ok());
        assert!(parser.push(b"data: g\n").is_err());
        // An event too large fails whether it arrives whole or in pieces, wherever the pieces are cut.
        let frame = b"data: abcdefghijklmnopqrstuvwxyz\n\n";
        for cut in [frame.len(), 6, 20, frame.len() - 1] {
            let mut parser = Parser::with_limit(16);
            let (first, rest) = frame.split_at(cut);
            let pushed = parser.push(first).and_then(|_| parser.push(rest));
            assert!(pushed.is_err(), "cut at {cut}");
        }
        // Events within the limit, however many.
        let mut parser = Parser::with_limit(16);
        for _ in 0..100 {
            assert_eq!(parser.push(b"data: 0123456789\n\n").unwrap().len(), 1);
        }
    }
}
