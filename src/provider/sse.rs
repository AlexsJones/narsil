//! Incremental Server-Sent Events decoder.

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
}

impl SseDecoder {
    /// Feed bytes as they arrive; returns every event completed by this chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            let mut line = &self.buf[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line).into_owned();
            start = end + 1;
            self.line(&line, &mut out);
        }
        self.buf.drain(..start);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if self.has_data {
                out.push(SseEvent {
                    event: std::mem::take(&mut self.event),
                    data: std::mem::take(&mut self.data),
                });
            }
            self.event.clear();
            self.has_data = false;
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_events_split_across_chunks() {
        let mut d = SseDecoder::default();
        let mut got = Vec::new();
        for chunk in ["event: a\nda", "ta: {\"x\":", "1}\n\n: ping\n\ndata: one\r\ndata: two\r\n\r\ndata: tail"] {
            got.extend(d.push(chunk.as_bytes()));
        }
        assert_eq!(
            got,
            vec![
                SseEvent { event: "a".into(), data: "{\"x\":1}".into() },
                SseEvent { event: "".into(), data: "one\ntwo".into() },
            ]
        );
        // The unterminated tail is held until its blank line arrives.
        assert_eq!(d.push(b"\n\n"), vec![SseEvent { event: "".into(), data: "tail".into() }]);
    }

    #[test]
    fn multibyte_characters_survive_chunk_boundaries() {
        let mut d = SseDecoder::default();
        let bytes = "data: héllo\n\n".as_bytes();
        let (a, b) = bytes.split_at(8); // splits inside 'é'
        let mut got = d.push(a);
        got.extend(d.push(b));
        assert_eq!(got[0].data, "héllo");
    }
}
