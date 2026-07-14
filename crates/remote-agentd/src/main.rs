//! Remote Agent Daemon — MCP stdio server for remote file operations.
//!
//! Reads newline-delimited JSON-RPC from stdin, dispatches via `McpHandler`,
//! and writes responses to stdout (flushed after each message batch).
//!
//! Uses synchronous `std::io` — no async runtime required. Stdio is
//! inherently sequential, so an async event loop adds no value here.
//!
//! Logging goes to stderr so it never corrupts the JSON-RPC stdout stream.

mod mcp;
mod tools;

use std::io::{self, Read, Write};

use serde_json::{json, Value};
use mcp::{json_error, McpHandler, PARSE_ERROR};

/// Maximum allowed line length (1 MiB). MCP JSON-RPC messages are small;
/// a line exceeding this almost certainly indicates a malformed/truncated
/// input (e.g. unbalanced quotes causing the reader to buffer past the
/// intended message boundary). We reject the line with a parse error instead
/// of buffering indefinitely.
const MAX_LINE_LEN: usize = 1024 * 1024;

/// Maximum number of consecutive parse errors before the daemon gives up.
/// This prevents infinite-output scenarios where a broken client keeps
/// sending garbage and the daemon keeps responding with parse errors.
const MAX_CONSECUTIVE_ERRORS: u32 = 100;

fn main() -> anyhow::Result<()> {
    // Wire up tool handlers. Each tool's static `execute` is wrapped in a
    // trait impl in `tools/mod.rs` and registered here.
    let mut handler = McpHandler::new();
    tools::register_all(&mut handler);

    let stdin = io::stdin();
    let stdout = io::stdout();
    let stdin_lock = stdin.lock();
    let mut stdout_lock = stdout.lock();

    let mut reader = LineReader::new(stdin_lock, MAX_LINE_LEN);
    let mut consecutive_errors: u32 = 0;

    loop {
        match reader.next_line() {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }

                let messages = handler.handle_message(&line);
                if messages.is_empty() {
                    continue;
                }

                // Reset error counter on any successful line read — we only
                // care about *consecutive* garbage, not total error count.
                consecutive_errors = 0;

                for msg in &messages {
                    let json = serde_json::to_string(msg)?;
                    writeln!(stdout_lock, "{json}")?;
                }
                stdout_lock.flush()?;
            }
            Ok(None) => {
                // Clean EOF — exit normally.
                break;
            }
            Err(LineError::TooLong(len)) => {
                consecutive_errors += 1;
                eprintln!(
                    "remote-agentd: line too long ({} bytes, limit {}), rejecting",
                    len, MAX_LINE_LEN
                );
                let err_msg = json!(json_error(
                    Value::Null,
                    PARSE_ERROR,
                    &format!(
                        "Line too long ({} bytes, limit {}). Input may be truncated/malformed.",
                        len, MAX_LINE_LEN
                    )
                ));
                writeln!(stdout_lock, "{err_msg}")?;
                stdout_lock.flush()?;

                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!(
                        "remote-agentd: {} consecutive errors, giving up",
                        consecutive_errors
                    );
                    break;
                }
            }
            Err(LineError::Io(e)) => {
                eprintln!("remote-agentd: stdin read error: {e}");
                break;
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Line reader with length limiting
// ─────────────────────────────────────────────────────────────────────────

/// A line reader that enforces a maximum line length.
///
/// Unlike `BufRead::lines()`, which buffers indefinitely, this reader
/// returns `LineError::TooLong` when a line exceeds the limit. This
/// prevents unbounded memory growth from malformed/truncated JSON input
/// (e.g. unbalanced quotes in a `printf` pipeline that cause the entire
/// remaining input to be treated as a single "line").
///
/// After a `TooLong` error, the reader discards the over-long line up to
/// and including the next newline, then resumes normal reading. This
/// allows recovery if the over-long line was a one-time anomaly (e.g. a
/// single truncated message followed by valid messages on the same stream).
struct LineReader<R: Read> {
    reader: R,
    max_len: usize,
    buf: Vec<u8>,
    /// Position in buf of the next unread byte.
    pos: usize,
}

/// Error from `LineReader::next_line`.
#[derive(Debug)]
enum LineError {
    /// The line exceeded `max_len` bytes. The actual length seen so far is
    /// included for diagnostics. The over-long data has been discarded up to
    /// and including the next newline.
    TooLong(usize),
    /// Underlying I/O error.
    Io(io::Error),
}

impl<R: Read> LineReader<R> {
    fn new(reader: R, max_len: usize) -> Self {
        Self {
            reader,
            max_len,
            buf: Vec::with_capacity(8192),
            pos: 0,
        }
    }

    /// Read the next line (without the trailing newline).
    ///
    /// Returns `Ok(Some(line))` for a complete line, `Ok(None)` for clean
    /// EOF (no more data), or `Err(LineError::TooLong)` if the line exceeds
    /// the limit.
    fn next_line(&mut self) -> Result<Option<String>, LineError> {
        loop {
            // Search for a newline in the unconsumed portion of buf.
            if self.pos < self.buf.len() {
                if let Some(nl_pos) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                    // Found a newline — extract the line.
                    let start = self.pos;
                    let end = self.pos + nl_pos;
                    let line_bytes = &self.buf[start..end];

                    // Check length *before* extracting.
                    if line_bytes.len() > self.max_len {
                        let len = line_bytes.len();
                        // Discard the over-long line.
                        self.pos = end + 1;
                        self.compact();
                        return Err(LineError::TooLong(len));
                    }

                    let line = String::from_utf8_lossy(line_bytes).into_owned();
                    // Strip trailing \r if present (CRLF support).
                    let line = if line.ends_with('\r') {
                        line[..line.len() - 1].to_string()
                    } else {
                        line
                    };

                    self.pos = end + 1;
                    self.compact();
                    return Ok(Some(line));
                }
            }

            // No newline in buffer — need to read more data.
            // First, check if the accumulated line (without newline) is
            // already over the limit. This catches the case where the input
            // has no newline at all (truncated, no EOF signal from SSH).
            let current_line_len = self.buf.len() - self.pos;
            if current_line_len > self.max_len {
                let len = current_line_len;
                // Discard everything up to the next newline or EOF.
                // Read and discard until we find a newline or hit EOF.
                self.discard_until_newline()?;
                return Err(LineError::TooLong(len));
            }

            // Read more data into the buffer.
            self.buf.resize(self.buf.len() + 8192, 0);
            let read_start = self.buf.len() - 8192;
            let n = match self.reader.read(&mut self.buf[read_start..]) {
                Ok(0) => {
                    // EOF — return any remaining data as a final line.
                    self.buf.truncate(self.buf.len() - 8192); // remove the zero-read padding
                    if self.pos < self.buf.len() {
                        let line_bytes = &self.buf[self.pos..];
                        if line_bytes.iter().all(|&b| b == b'\r' || b == b'\n') {
                            return Ok(None); // trailing whitespace only
                        }
                        if line_bytes.len() > self.max_len {
                            let len = line_bytes.len();
                            self.pos = self.buf.len();
                            return Err(LineError::TooLong(len));
                        }
                        let line = String::from_utf8_lossy(line_bytes).into_owned();
                        let line = if line.ends_with('\r') {
                            line[..line.len() - 1].to_string()
                        } else {
                            line
                        };
                        self.pos = self.buf.len();
                        return Ok(Some(line));
                    }
                    return Ok(None);
                }
                Ok(n) => n,
                Err(e) => {
                    self.buf.truncate(self.buf.len() - 8192); // remove padding
                    return Err(LineError::Io(e));
                }
            };
            self.buf.truncate(read_start + n); // trim to actual bytes read
        }
    }

    /// Discard buffered data up to and including the next newline.
    /// Reads more from the underlying reader if necessary.
    fn discard_until_newline(&mut self) -> Result<(), LineError> {
        // First check existing buffer.
        if self.pos < self.buf.len() {
            if let Some(nl_pos) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                self.pos += nl_pos + 1;
                self.compact();
                return Ok(());
            }
        }

        // Not in buffer — read until we find one or hit EOF.
        let mut tmp = [0u8; 8192];
        loop {
            match self.reader.read(&mut tmp) {
                Ok(0) => {
                    // EOF without newline — discard everything.
                    self.pos = self.buf.len();
                    self.compact();
                    return Ok(());
                }
                Ok(n) => {
                    if let Some(nl) = tmp[..n].iter().position(|&b| b == b'\n') {
                        // Found newline — the remaining data after it goes
                        // into the buffer for the next next_line() call.
                        self.buf.extend_from_slice(&tmp[..n]);
                        self.pos = self.buf.len() - (n - nl - 1);
                        self.compact();
                        return Ok(());
                    }
                    // No newline yet — keep reading and discarding.
                }
                Err(e) => return Err(LineError::Io(e)),
            }
        }
    }

    /// Move unconsumed data to the front of the buffer and trim the rest.
    fn compact(&mut self) {
        if self.pos > 0 {
            if self.pos < self.buf.len() {
                self.buf.copy_within(self.pos.., 0);
            }
            self.buf.truncate(self.buf.len() - self.pos);
            self.pos = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_reader(input: &str, max_len: usize) -> LineReader<Cursor<&str>> {
        LineReader::new(Cursor::new(input), max_len)
    }

    #[test]
    fn line_reader_basic() {
        let mut r = make_reader("hello\nworld\n", 1024);
        assert_eq!(r.next_line().unwrap().unwrap(), "hello");
        assert_eq!(r.next_line().unwrap().unwrap(), "world");
        assert!(r.next_line().unwrap().is_none()); // EOF
    }

    #[test]
    fn line_reader_no_trailing_newline() {
        let mut r = make_reader("hello\nworld", 1024);
        assert_eq!(r.next_line().unwrap().unwrap(), "hello");
        assert_eq!(r.next_line().unwrap().unwrap(), "world");
        assert!(r.next_line().unwrap().is_none());
    }

    #[test]
    fn line_reader_empty_input() {
        let mut r = make_reader("", 1024);
        assert!(r.next_line().unwrap().is_none());
    }

    #[test]
    fn line_reader_blank_lines() {
        let mut r = make_reader("\n\n\n", 1024);
        assert_eq!(r.next_line().unwrap().unwrap(), "");
        assert_eq!(r.next_line().unwrap().unwrap(), "");
        assert_eq!(r.next_line().unwrap().unwrap(), "");
        assert!(r.next_line().unwrap().is_none());
    }

    #[test]
    fn line_reader_crlf() {
        let mut r = make_reader("hello\r\nworld\r\n", 1024);
        assert_eq!(r.next_line().unwrap().unwrap(), "hello");
        assert_eq!(r.next_line().unwrap().unwrap(), "world");
    }

    #[test]
    fn line_reader_unicode() {
        let mut r = make_reader("héllo\n世界\n", 1024);
        assert_eq!(r.next_line().unwrap().unwrap(), "héllo");
        assert_eq!(r.next_line().unwrap().unwrap(), "世界");
    }

    #[test]
    fn line_reader_rejects_oversized_line() {
        // Line of 200 bytes, limit 100.
        let long_line = "x".repeat(200);
        let input = format!("{}\nshort\n", long_line);
        let mut r = make_reader(&input, 100);

        let err = r.next_line().unwrap_err();
        match err {
            LineError::TooLong(len) => assert_eq!(len, 200),
            _ => panic!("expected TooLong"),
        }

        // After the error, the reader should recover and read the next line.
        assert_eq!(r.next_line().unwrap().unwrap(), "short");
    }

    #[test]
    fn line_reader_oversized_no_newline_then_eof() {
        // Oversized line with no newline at all — should error then EOF.
        let long_line = "x".repeat(200);
        let mut r = make_reader(&long_line, 100);

        let err = r.next_line().unwrap_err();
        match err {
            LineError::TooLong(len) => assert!(len >= 200),
            _ => panic!("expected TooLong"),
        }

        // Should now be at EOF.
        assert!(r.next_line().unwrap().is_none());
    }

    #[test]
    fn line_reader_oversized_then_recover_multiple() {
        // Multiple oversized lines followed by valid ones.
        let input = format!(
            "{}\n{}\n{}\nvalid\n",
            "x".repeat(50),
            "x".repeat(50),
            "x".repeat(50)
        );
        let mut r = make_reader(&input, 20);

        // Three oversized lines → three errors.
        for _ in 0..3 {
            assert!(matches!(r.next_line(), Err(LineError::TooLong(_))));
        }

        // Then a valid line.
        assert_eq!(r.next_line().unwrap().unwrap(), "valid");
    }

    #[test]
    fn line_reader_large_but_under_limit() {
        // Line of exactly 100 bytes, limit 100 — should be OK.
        let line = "x".repeat(100);
        let input = format!("{}\n", line);
        let mut r = make_reader(&input, 100);
        assert_eq!(r.next_line().unwrap().unwrap(), line);
    }

    #[test]
    fn line_reader_just_over_limit() {
        // Line of 101 bytes, limit 100 — should error.
        let line = "x".repeat(101);
        let input = format!("{}\n", line);
        let mut r = make_reader(&input, 100);
        assert!(matches!(r.next_line(), Err(LineError::TooLong(_))));
    }

    #[test]
    fn line_reader_multiple_reads_needed() {
        // Input where the newline is far in — simulates multiple read() calls.
        let mut input = String::new();
        for i in 0..100 {
            input.push_str(&format!("line{}\n", i));
        }
        let mut r = make_reader(&input, 1024);
        for i in 0..100 {
            assert_eq!(r.next_line().unwrap().unwrap(), format!("line{}", i));
        }
        assert!(r.next_line().unwrap().is_none());
    }

    #[test]
    fn line_reader_partial_line_then_more_data() {
        // Simulate: "hello" (no newline yet) then more data arrives.
        // Since we use Cursor (all data available at once), this tests
        // that the reader correctly handles a line split across buffer fills.
        let input = "hello world this is a long line that spans multiple buffer reads\nnext\n";
        let mut r = make_reader(input, 1024);
        let line = r.next_line().unwrap().unwrap();
        assert_eq!(line, "hello world this is a long line that spans multiple buffer reads");
        assert_eq!(r.next_line().unwrap().unwrap(), "next");
    }
}
