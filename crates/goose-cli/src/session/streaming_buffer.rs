//! Streaming markdown buffer for safe incremental rendering.
//!
//! This module provides a buffer that accumulates streaming markdown chunks
//! and determines points to flush content for rendering based on empty lines.
//!
//! # Example
//!
//! ```
//! use goose_cli::session::streaming_buffer::MarkdownBuffer;
//!
//! let mut buf = MarkdownBuffer::new();
//!
//! // Content is buffered until an empty line (double newline)
//! assert_eq!(buf.push("Hello\n"), None);
//! assert_eq!(buf.push("\nWorld"), Some("Hello\n\n".to_string()));
//!
//! // At end of stream, flush remaining content
//! let remaining = buf.flush();
//! assert_eq!(remaining, "World".to_string());
//! ```

/// A streaming markdown buffer that accumulates chunks and flushes on empty lines.
#[derive(Default)]
pub struct MarkdownBuffer {
    buffer: String,
}

impl MarkdownBuffer {
    /// Create a new empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a chunk of markdown text to the buffer.
    ///
    /// Returns any content up to the last empty line (double newline),
    /// or None if no empty line is present in the buffer.
    pub fn push(&mut self, chunk: &str) -> Option<String> {
        self.buffer.push_str(chunk);

        // Find the last occurrence of double newline
        if let Some(last_empty_line) = self.buffer.rfind("\n\n") {
            let split_pos = last_empty_line + 2;
            let to_render = self.buffer[..split_pos].to_string();
            self.buffer = self.buffer[split_pos..].to_string();
            Some(to_render)
        } else {
            None
        }
    }

    /// Flush any remaining content from the buffer.
    pub fn flush(&mut self) -> String {
        std::mem::take(&mut self.buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_buffering() {
        let mut buf = MarkdownBuffer::new();
        assert_eq!(buf.push("Paragraph 1\n"), None);
        assert_eq!(buf.push("\nParagraph 2"), Some("Paragraph 1\n\n".to_string()));
        assert_eq!(buf.flush(), "Paragraph 2".to_string());
    }

    #[test]
    fn test_multiple_empty_lines() {
        let mut buf = MarkdownBuffer::new();
        let output = buf.push("Para 1\n\nPara 2\n\nPara 3");
        assert_eq!(output, Some("Para 1\n\nPara 2\n\n".to_string()));
        assert_eq!(buf.flush(), "Para 3".to_string());
    }

    #[test]
    fn test_no_empty_line() {
        let mut buf = MarkdownBuffer::new();
        assert_eq!(buf.push("Just some text"), None);
        assert_eq!(buf.push(" without empty lines"), None);
        assert_eq!(buf.flush(), "Just some text without empty lines".to_string());
    }
}
