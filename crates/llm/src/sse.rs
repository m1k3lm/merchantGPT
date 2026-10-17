//! Minimal Server-Sent Events parser over a byte stream.

use std::collections::VecDeque;

use futures::{Stream, StreamExt};

use crate::{EventStream, LlmError, StreamEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser. Bytes are buffered until a blank line ends an event,
/// so chunk boundaries (including ones inside UTF-8 sequences) are harmless.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep_len)) = find_event_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end + sep_len).take(end).collect();
            if let Some(ev) = parse_event(&String::from_utf8_lossy(&raw)) {
                out.push(ev);
            }
        }
        out
    }

    /// Parses whatever is left when the stream ends without a trailing blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let raw = std::mem::take(&mut self.buf);
        parse_event(&String::from_utf8_lossy(&raw))
    }
}

fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

fn parse_event(raw: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }
    (event.is_some() || !data.is_empty()).then(|| SseEvent {
        event,
        data: data.join("\n"),
    })
}

/// Adapts an HTTP byte stream into a stream of SSE events.
pub fn sse_events<S, B, E>(bytes: S) -> impl Stream<Item = Result<SseEvent, LlmError>> + Send
where
    S: Stream<Item = Result<B, E>> + Send + Unpin + 'static,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    struct State<S> {
        bytes: S,
        parser: SseParser,
        pending: VecDeque<SseEvent>,
        done: bool,
    }
    futures::stream::unfold(
        State {
            bytes,
            parser: SseParser::default(),
            pending: VecDeque::new(),
            done: false,
        },
        |mut st| async move {
            loop {
                if let Some(ev) = st.pending.pop_front() {
                    return Some((Ok(ev), st));
                }
                if st.done {
                    return None;
                }
                match st.bytes.next().await {
                    Some(Ok(chunk)) => st.pending.extend(st.parser.feed(chunk.as_ref())),
                    Some(Err(e)) => {
                        st.done = true;
                        return Some((Err(LlmError::Transport(e.to_string())), st));
                    }
                    None => {
                        st.done = true;
                        st.pending.extend(st.parser.finish());
                    }
                }
            }
        },
    )
}

/// A provider-specific translator from SSE events to `StreamEvent`s.
pub(crate) trait SseDecoder: Send + 'static {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<StreamEvent>, LlmError>;
    /// Called once when the byte stream ends; must emit `Done` if not yet sent.
    fn finish(&mut self) -> Vec<StreamEvent>;
}

/// Runs a decoder over an SSE stream, stopping after the first error.
pub(crate) fn decode_stream<S, D>(events: S, decoder: D) -> EventStream
where
    S: Stream<Item = Result<SseEvent, LlmError>> + Send + 'static,
    D: SseDecoder,
{
    struct State<S, D> {
        events: std::pin::Pin<Box<S>>,
        decoder: D,
        pending: VecDeque<Result<StreamEvent, LlmError>>,
        done: bool,
    }
    futures::stream::unfold(
        State {
            events: Box::pin(events),
            decoder,
            pending: VecDeque::new(),
            done: false,
        },
        |mut st| async move {
            loop {
                if let Some(ev) = st.pending.pop_front() {
                    if ev.is_err() {
                        st.done = true;
                        st.pending.clear();
                    }
                    return Some((ev, st));
                }
                if st.done {
                    return None;
                }
                match st.events.next().await {
                    Some(Ok(ev)) => match st.decoder.on_event(&ev) {
                        Ok(out) => st.pending.extend(out.into_iter().map(Ok)),
                        Err(e) => st.pending.push_back(Err(e)),
                    },
                    Some(Err(e)) => st.pending.push_back(Err(e)),
                    None => {
                        st.done = true;
                        st.pending.extend(st.decoder.finish().into_iter().map(Ok));
                    }
                }
            }
        },
    )
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_splits_events_across_chunk_boundaries() {
        let mut p = SseParser::default();
        assert!(p.feed(b"event: a\nda").is_empty());
        assert!(p.feed(b"ta: {\"x\":").is_empty());
        let evs = p.feed(b"1}\n\ndata: second\r\n\r\n: comment\n\n");
        assert_eq!(
            evs,
            vec![
                SseEvent {
                    event: Some("a".into()),
                    data: "{\"x\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "second".into()
                },
            ]
        );
    }

    #[test]
    fn multibyte_utf8_split_across_chunks_survives() {
        let mut p = SseParser::default();
        let bytes = "data: café\n\n".as_bytes();
        let split = bytes.iter().position(|&b| b == 0xC3).unwrap() + 1;
        assert!(p.feed(&bytes[..split]).is_empty());
        assert_eq!(p.feed(&bytes[split..])[0].data, "café");
    }

    #[test]
    fn multiple_data_lines_join_with_newline_and_finish_flushes_tail() {
        let mut p = SseParser::default();
        assert!(p.feed(b"data: a\ndata: b").is_empty());
        assert_eq!(p.finish().unwrap().data, "a\nb");
    }

    #[tokio::test]
    async fn sse_events_adapts_a_byte_stream() {
        let chunks: Vec<Result<&'static [u8], String>> =
            vec![Ok(b"data: 1\n\nda"), Ok(b"ta: 2\n\n"), Ok(b"data: 3")];
        let evs: Vec<_> = sse_events(futures::stream::iter(chunks))
            .map(|e| e.unwrap().data)
            .collect()
            .await;
        assert_eq!(evs, ["1", "2", "3"]);
    }
}
