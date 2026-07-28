//! Provider 流式事件和不依赖厂商协议的 SSE 分帧器。

use async_trait::async_trait;

use crate::error::{XduduError, XduduResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    TextDelta { text: String },
}

#[async_trait]
pub trait ProviderStreamSink: Send + Sync {
    async fn emit(&self, event: ProviderStreamEvent);
}

#[derive(Debug, Default)]
pub(super) struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> XduduResult<Vec<String>> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some((position, delimiter_len)) = delimiter(&self.buffer) {
            let frame = self.buffer.drain(..position).collect::<Vec<_>>();
            self.buffer.drain(..delimiter_len);
            if let Some(data) = frame_data(&frame)? {
                events.push(data);
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> XduduResult<Vec<String>> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        let frame = std::mem::take(&mut self.buffer);
        Ok(frame_data(&frame)?.into_iter().collect())
    }
}

fn delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|part| part == b"\n\n");
    let crlf = buffer.windows(4).position(|part| part == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(left), Some(right)) if left <= right => Some((left, 2)),
        (Some(_), Some(right)) => Some((right, 4)),
        (Some(left), None) => Some((left, 2)),
        (None, Some(right)) => Some((right, 4)),
        (None, None) => None,
    }
}

fn frame_data(frame: &[u8]) -> XduduResult<Option<String>> {
    let text = std::str::from_utf8(frame)
        .map_err(|error| XduduError::provider(format!("SSE 包含无效 UTF-8：{error}"), false))?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>();
    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(data.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_支持跨块_utf8_和_crlf() {
        let raw = "data: {\"text\":\"你好\"}\r\n\r\n".as_bytes();
        let split = raw.len() - 5;
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(&raw[..split]).unwrap().is_empty());
        let events = decoder.push(&raw[split..]).unwrap();
        assert_eq!(events, vec!["{\"text\":\"你好\"}"]);
    }

    #[test]
    fn sse_合并多行_data() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b"event: value\ndata: first\ndata: second\n\n")
            .unwrap();
        assert_eq!(events, vec!["first\nsecond"]);
    }
}
