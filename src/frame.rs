//! Bounded NDJSON frame reader for the ACP stdio transport.
//!
//! `AsyncBufReadExt::lines()` / `read_line` allocate an unbounded
//! `String` per call — a peer that never sends `\n` (or sends a
//! multi-GB line) grows memory without limit before any application
//! limit (`LLM_MAX_PROMPT_CHARS`) ever runs. This reader caps each
//! frame at [`MAX_FRAME_BYTES`]; an oversized frame is drained to its
//! newline and reported as [`Frame::Oversized`] so the loop can resync
//! on the next frame instead of buffering the payload.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Largest accepted single frame. Sized well above the
/// `LLM_MAX_PROMPT_CHARS` admission bound (200k chars ≈ ≤800 KiB of
/// UTF-8 even at 4 bytes/char) so legitimate prompts — including
/// base64 image blocks — pass through; anything larger is transport
/// garbage, not a request we could serve.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Result of one [`read_frame`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// Complete line, newline stripped. May still fail JSON parsing —
    /// that is the caller's concern, not the transport's.
    Line(Vec<u8>),
    /// A frame exceeded [`MAX_FRAME_BYTES`]; its bytes were discarded
    /// through the terminating newline (or EOF). The peer cannot be
    /// answered (its id is inside the dropped payload), so callers log
    /// and continue.
    Oversized,
    /// Clean EOF with no partial frame pending.
    Eof,
}

/// Read one newline-terminated frame with a hard byte cap.
///
/// `buf` is scratch space reused across calls — pass a `Vec::new()`
/// once and keep it; it is cleared on entry.
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Frame> {
    buf.clear();
    // True once the current frame has blown the cap: we keep draining
    // to the newline but stop accumulating.
    let mut draining = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if draining {
                Frame::Oversized
            } else if buf.is_empty() {
                Frame::Eof
            } else {
                // EOF mid-line: hand the partial bytes to the caller;
                // JSON parse fails and the loop moves on.
                Frame::Line(std::mem::take(buf))
            });
        }
        let len = chunk.len();
        let nl = chunk.iter().position(|&b| b == b'\n');
        // Accumulate while `chunk` is still borrowed, then end the
        // borrow before consuming.
        let was_draining = draining;
        let mut oversized_now = false;
        if !draining {
            let room = MAX_FRAME_BYTES.saturating_sub(buf.len());
            match nl {
                Some(pos) if pos <= room => buf.extend_from_slice(&chunk[..pos]),
                Some(_) => oversized_now = true,
                None if len <= room => buf.extend_from_slice(chunk),
                None => {
                    buf.extend_from_slice(&chunk[..room]);
                    draining = true;
                }
            }
        }
        match nl {
            Some(pos) => {
                reader.consume(pos + 1);
                if was_draining || oversized_now {
                    return Ok(Frame::Oversized);
                }
                return Ok(Frame::Line(std::mem::take(buf)));
            }
            None => reader.consume(len),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn next<R: AsyncBufRead + Unpin>(r: &mut R) -> Frame {
        let mut buf = Vec::new();
        read_frame(r, &mut buf).await.unwrap()
    }

    #[tokio::test]
    async fn normal_lines_pass_through() {
        let mut r = tokio::io::BufReader::new(&b"{\"a\":1}\n{\"b\":2}\n"[..]);
        assert_eq!(next(&mut r).await, Frame::Line(b"{\"a\":1}".to_vec()));
        assert_eq!(next(&mut r).await, Frame::Line(b"{\"b\":2}".to_vec()));
        assert_eq!(next(&mut r).await, Frame::Eof);
    }

    #[tokio::test]
    async fn empty_lines_and_crlf() {
        let mut r = tokio::io::BufReader::new(&b"\n{\"x\":1}\r\n"[..]);
        assert_eq!(next(&mut r).await, Frame::Line(Vec::new()));
        // \r is part of the frame — JSON parsing trims it downstream.
        assert_eq!(next(&mut r).await, Frame::Line(b"{\"x\":1}\r".to_vec()));
    }

    #[tokio::test]
    async fn oversized_frame_is_dropped_and_stream_resyncs() {
        let big = "x".repeat(MAX_FRAME_BYTES + 100);
        let input = format!("{big}\n{{\"ok\":true}}\n");
        let mut r = tokio::io::BufReader::new(input.as_bytes());
        assert_eq!(next(&mut r).await, Frame::Oversized);
        assert_eq!(next(&mut r).await, Frame::Line(b"{\"ok\":true}".to_vec()));
        assert_eq!(next(&mut r).await, Frame::Eof);
    }

    #[tokio::test]
    async fn oversized_without_newline_until_eof() {
        let big = "y".repeat(MAX_FRAME_BYTES * 2);
        let bytes = big.into_bytes();
        let mut r = &bytes[..];
        assert_eq!(next(&mut r).await, Frame::Oversized);
    }

    #[tokio::test]
    async fn partial_line_at_eof_is_returned() {
        let mut r = tokio::io::BufReader::new(&b"{\"partial\":"[..]);
        assert_eq!(next(&mut r).await, Frame::Line(b"{\"partial\":".to_vec()));
        assert_eq!(next(&mut r).await, Frame::Eof);
    }
}
