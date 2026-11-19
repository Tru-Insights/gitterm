//! Bounded observation of OSC 7 and shell prompt readiness; no PTY bytes are consumed.
use crate::events::EventProxy;
use std::path::PathBuf;

#[derive(Default)]
pub(crate) struct Reports {
    state: u8,
    bytes: Vec<u8>,
    overflow: bool,
}
impl Reports {
    pub fn advance(&mut self, input: &[u8], events: &EventProxy) {
        // Ordinary output has no report delimiters. Slice::contains uses the
        // platform byte scan, avoiding a second per-byte debug parser on floods.
        if self.state == 0 && !input.contains(&0x1b) {
            return;
        }
        for &byte in input {
            match (self.state, byte) {
                (_, 0x18 | 0x1a) => {
                    self.state = 0;
                    self.bytes.clear();
                }
                (0, 0x1b) => self.state = 1,
                (1 | 3 | 5, b']') => {
                    self.state = 2;
                    self.bytes.clear();
                    self.overflow = false;
                }
                (1 | 3 | 5, b'P' | b'X' | b'^' | b'_') => self.state = 4,
                (1 | 3 | 5, 0x1b) => self.state = 1,
                (1, _) => self.state = 0,
                (2, 7) | (3, b'\\') => {
                    if !self.overflow {
                        self.dispatch(events);
                    }
                    self.state = 0;
                    self.bytes.clear();
                }
                (2, 0x1b) => self.state = 3,
                (2, _) => {
                    if self.bytes.len() < 4096 {
                        self.bytes.push(byte);
                    } else {
                        self.overflow = true;
                    }
                }
                (3, _) => {
                    self.bytes.clear();
                    self.state = 0;
                }
                (4, 0x1b) => self.state = 5,
                (5, _) => self.state = 0,
                _ => {}
            }
        }
    }
    fn dispatch(&self, events: &EventProxy) {
        if self.bytes == b"133;B" {
            events.prompt_ready();
        }
        if let Some(uri) = self.bytes.strip_prefix(b"7;")
            && let Some(path) = cwd_uri(uri)
        {
            events.report_cwd(path);
        }
    }
}
fn cwd_uri(uri: &[u8]) -> Option<PathBuf> {
    let uri = std::str::from_utf8(uri).ok()?.strip_prefix("file://")?;
    let start = uri.find('/')?;
    // OSC 7's host is metadata only. Never open it or resolve it over a network.
    let path = &uri.as_bytes()[start..];
    let mut decoded = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        if path[i] == b'%' {
            let hex = std::str::from_utf8(path.get(i + 1..i + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(path[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(decoded).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_escapes_recover_into_a_fresh_report() {
        for prefix in [
            b"\x1b".as_slice(),
            b"\x1b]unfinished",
            b"\x1bPunfinished",
            b"\x1b_ignored",
        ] {
            let events = EventProxy::default();
            let mut parser = Reports::default();
            parser.advance(prefix, &events);
            for byte in b"\x1b]7;file:///recovered\x07" {
                parser.advance(&[*byte], &events);
            }
            assert_eq!(events.take().cwd, Some(PathBuf::from("/recovered")));
        }
        let events = EventProxy::default();
        let mut parser = Reports::default();
        parser.advance(b"\x1b]", &events);
        parser.advance(&vec![b'x'; 8192], &events);
        parser.advance(b"\x1b]7;file:///after-overflow\x1b\\", &events);
        assert_eq!(events.take().cwd, Some(PathBuf::from("/after-overflow")));
    }
    #[test]
    fn split_bel_st_invalid_and_oversized_reports() {
        let events = EventProxy::default();
        let mut parser = Reports::default();
        for byte in b"\x1b]7;file://host/tmp/a%20b\x1b\\" {
            parser.advance(&[*byte], &events);
        }
        assert_eq!(events.take().cwd, Some(PathBuf::from("/tmp/a b")));
        parser.advance(
            b"\x1b]7;file:///bad%00\x07\x1b]7;file:///bad%GG\x07",
            &events,
        );
        assert!(events.take().cwd.is_none());
        parser.advance(b"\x1b]7;file:///", &events);
        parser.advance(&vec![b'a'; 8192], &events);
        parser.advance(b"\x07", &events);
        assert!(events.take().cwd.is_none());
        parser.advance(b"\x1b]7;file:///ok\x07", &events);
        assert_eq!(events.take().cwd, Some(PathBuf::from("/ok")));
    }
}
