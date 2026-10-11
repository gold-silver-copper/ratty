//! Bounded host control strings and synchronized presentation frames.

use fux_vt::{Mode, Parser, Size};
struct IgnoreReplies;
impl fux_vt::Sink for IgnoreReplies {}
use std::time::{Duration, Instant};

/// A graphics command must be chunked below this encoded size. A transfer can
/// span many commands; its decoded limit is enforced by the graphics parser.
pub(crate) const APC_LIMIT: usize = 1024 * 1024;
const FRAME_LIMIT: usize = 2 * 1024 * 1024;
const FRAME_PIECES: usize = 4096;
pub(crate) const FRAME_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    Text(Vec<u8>),
    Apc(Vec<u8>),
    DiscardGraphics,
}
impl Piece {
    fn len(&self) -> usize {
        match self {
            Self::Text(bytes) | Self::Apc(bytes) => bytes.len(),
            Self::DiscardGraphics => 0,
        }
    }
}

#[derive(Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Apc {
        bytes: Vec<u8>,
        escape: bool,
        utf8: u8,
    },
    Discard {
        escape: bool,
        utf8: u8,
    },
}

fn utf8_next(pending: u8, byte: u8) -> u8 {
    match byte {
        0x80..=0xbf if pending != 0 => pending - 1,
        0xc2..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf4 => 3,
        _ => 0,
    }
}

#[derive(Default)]
pub(crate) struct ApcScanner {
    state: ScanState,
}
impl ApcScanner {
    pub(crate) fn feed(&mut self, input: &[u8]) -> Vec<Piece> {
        let mut pieces = Vec::new();
        let mut text = Vec::new();
        for &byte in input {
            self.state = match std::mem::take(&mut self.state) {
                ScanState::Ground if byte == 0x1b => ScanState::Escape,
                ScanState::Ground => {
                    text.push(byte);
                    ScanState::Ground
                }
                ScanState::Escape if byte == b'_' => {
                    if !text.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut text)));
                    }
                    ScanState::Apc {
                        bytes: b"\x1b_".to_vec(),
                        escape: false,
                        utf8: 0,
                    }
                }
                ScanState::Escape if byte == 0x1b => {
                    text.push(0x1b);
                    ScanState::Escape
                }
                ScanState::Escape => {
                    text.extend_from_slice(&[0x1b, byte]);
                    ScanState::Ground
                }
                ScanState::Apc {
                    mut bytes,
                    escape,
                    utf8,
                } => {
                    if matches!(byte, 0x18 | 0x1a) {
                        pieces.push(Piece::DiscardGraphics);
                        ScanState::Ground
                    } else if bytes.len() == APC_LIMIT {
                        pieces.push(Piece::DiscardGraphics);
                        if (escape && byte == b'\\') || (byte == 0x9c && utf8 == 0) {
                            ScanState::Ground
                        } else {
                            ScanState::Discard {
                                escape: byte == 0x1b,
                                utf8: utf8_next(utf8, byte),
                            }
                        }
                    } else {
                        bytes.push(byte);
                        if (escape && byte == b'\\') || (byte == 0x9c && utf8 == 0) {
                            pieces.push(Piece::Apc(bytes));
                            ScanState::Ground
                        } else {
                            ScanState::Apc {
                                bytes,
                                escape: byte == 0x1b,
                                utf8: utf8_next(utf8, byte),
                            }
                        }
                    }
                }
                ScanState::Discard { escape, utf8 } => {
                    // Once oversized, ignore even nested introductions through
                    // a terminator. Payload bytes must never become screen text.
                    if (escape && byte == b'\\')
                        || (byte == 0x9c && utf8 == 0)
                        || matches!(byte, 0x18 | 0x1a)
                    {
                        ScanState::Ground
                    } else {
                        ScanState::Discard {
                            escape: byte == 0x1b,
                            utf8: utf8_next(utf8, byte),
                        }
                    }
                }
            };
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        pieces
    }

    pub(crate) fn finish(&mut self) -> Vec<Piece> {
        match std::mem::take(&mut self.state) {
            ScanState::Escape => vec![Piece::Text(vec![0x1b])],
            ScanState::Apc { .. } | ScanState::Discard { .. } => vec![Piece::DiscardGraphics],
            _ => Vec::new(),
        }
    }
}

struct HeldFrame {
    pieces: Vec<Piece>,
    bytes: usize,
    since: Instant,
}

pub(crate) struct Frames {
    // Only control-state interpretation is needed here. No scrollback or
    // graphics payload lives in this one-cell observer. The authoritative
    // parser receives only committed pieces, as do the graphics/scene paths.
    probe: Parser,
    held: Option<HeldFrame>,
}
impl Default for Frames {
    fn default() -> Self {
        Self {
            probe: Parser::new(Size::new(1, 1).expect("one-cell frame observer"), 0)
                .expect("one-cell parser"),
            held: None,
        }
    }
}
impl Frames {
    pub(crate) fn pending(&self) -> bool {
        self.held.is_some()
    }
    pub(crate) fn release(&mut self) -> Vec<Piece> {
        self.held.take().map_or_else(Vec::new, |held| held.pieces)
    }
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<Piece> {
        if self
            .held
            .as_ref()
            .is_some_and(|held| now.saturating_duration_since(held.since) >= FRAME_TIMEOUT)
        {
            self.release()
        } else {
            Vec::new()
        }
    }
    fn append(&mut self, piece: Piece, out: &mut Vec<Piece>) {
        if self.held.as_ref().is_some_and(|held| {
            held.bytes.saturating_add(piece.len()) > FRAME_LIMIT
                || held.pieces.len() >= FRAME_PIECES
        }) {
            out.extend(self.release());
        }
        if let Some(held) = &mut self.held {
            held.bytes += piece.len();
            if let Piece::Text(bytes) = piece {
                if let Some(Piece::Text(previous)) = held.pieces.last_mut() {
                    previous.extend(bytes);
                } else {
                    held.pieces.push(Piece::Text(bytes));
                }
            } else {
                held.pieces.push(piece);
            }
        } else {
            out.push(piece);
        }
    }
    pub(crate) fn feed(&mut self, piece: Piece, now: Instant) -> Vec<Piece> {
        let mut out = self.expire(now);
        let Piece::Text(bytes) = piece else {
            if matches!(piece, Piece::Apc(_) | Piece::DiscardGraphics) {
                // An APC introduction cancels an incomplete VT sequence, but
                // its payload belongs only to the graphics parser.
                let _ = self.probe.process(b"\x1b_\x1b\\");
            }
            self.append(piece, &mut out);
            return out;
        };
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            if self.held.is_none() {
                match self
                    .probe
                    .process_until_frame(remaining, &mut IgnoreReplies)
                {
                    Ok(Some(taken)) => {
                        out.push(Piece::Text(remaining[..taken].to_vec()));
                        self.held = Some(HeldFrame {
                            pieces: Vec::new(),
                            bytes: 0,
                            since: now,
                        });
                        remaining = &remaining[taken..];
                    }
                    Ok(None) => {
                        out.push(Piece::Text(remaining.to_vec()));
                        break;
                    }
                    Err(error) => {
                        bevy::log::warn!("frame observer rejected output: {error}");
                        out.push(Piece::Text(remaining.to_vec()));
                        break;
                    }
                }
            } else {
                // Only an ESU ('l'), RIS ('c'), or DECSTR ('p') can release a
                // frame. Feed through those candidates, letting fux-vt decide
                // whether they are controls, text, or inside another string.
                // The scan advances once; growing held data is never rescanned.
                let count = remaining
                    .iter()
                    .position(|b| matches!(b, b'l' | b'c' | b'p'))
                    .map_or(remaining.len(), |i| i + 1);
                let (part, tail) = remaining.split_at(count);
                let mut probe_part = part;
                loop {
                    match self
                        .probe
                        .process_until_frame(probe_part, &mut IgnoreReplies)
                    {
                        Ok(Some(taken)) => probe_part = &probe_part[taken..],
                        Ok(None) => break,
                        Err(error) => {
                            bevy::log::warn!(
                                "frame observer failed; publishing held output: {error}"
                            );
                            out.extend(self.release());
                            break;
                        }
                    }
                }
                self.append(Piece::Text(part.to_vec()), &mut out);
                if !self.probe.screen().mode(Mode::SynchronizedOutput) {
                    out.extend(self.release());
                }
                remaining = tail;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(pieces: &[Piece]) -> Vec<u8> {
        pieces
            .iter()
            .flat_map(|p| match p {
                Piece::Text(b) => b.as_slice(),
                _ => &[],
            })
            .copied()
            .collect()
    }

    #[test]
    fn apc_is_split_safe_and_never_rescans_or_exposes_oversize_payloads() {
        let mut scanner = ApcScanner::default();
        assert_eq!(text(&scanner.feed(b"hi\x1b")), b"hi");
        assert!(scanner.feed(b"_G;aaa").is_empty());
        assert!(scanner.feed(b"\x1b").is_empty());
        assert_eq!(
            scanner.feed(b"\\"),
            vec![Piece::Apc(b"\x1b_G;aaa\x1b\\".to_vec())]
        );
        scanner.feed(b"\x1b_");
        for _ in 0..APC_LIMIT / 1024 {
            scanner.feed(&[b'x'; 1024]);
        }
        assert!(matches!(scanner.state, ScanState::Discard { .. }));
        assert!(scanner.feed(b"\x1b_G;hidden").is_empty());
        assert_eq!(text(&scanner.feed(b"\x1b\\visible")), b"visible");
    }

    #[test]
    fn an_apc_introduction_cancels_an_unfinished_osc() {
        let mut scanner = ApcScanner::default();
        let input = b"\x1b]0;title\x1b_G;fake\x1b\\";
        let pieces = scanner.feed(input);
        assert_eq!(text(&pieces), b"\x1b]0;title");
        assert_eq!(
            pieces.last(),
            Some(&Piece::Apc(b"\x1b_G;fake\x1b\\".to_vec()))
        );
    }

    #[test]
    fn utf8_continuation_bytes_are_not_eight_bit_string_terminators() {
        let mut scanner = ApcScanner::default();
        let input = "\x1b_ratty;g;r;path=✜.obj\x1b\\";
        let mut pieces = Vec::new();
        for byte in input.bytes() {
            pieces.extend(scanner.feed(&[byte]));
        }
        assert_eq!(pieces, [Piece::Apc(input.as_bytes().to_vec())]);
        assert_eq!(
            scanner.feed(b"\x1b_G;data\x9c"),
            [Piece::Apc(b"\x1b_G;data\x9c".to_vec())]
        );
    }

    #[test]
    fn frames_hold_text_and_graphics_at_every_split_and_resume_after_reset() {
        let input = b"old\x1b[?2026;25hnew\x1b_G;\x1b[?2026l\x1b\\more\x1b[?2026lafter";
        for split in 0..=input.len() {
            let mut scanner = ApcScanner::default();
            let mut frames = Frames::default();
            let now = Instant::now();
            let mut committed = Vec::new();
            for bytes in [&input[..split], &input[split..]] {
                for piece in scanner.feed(bytes) {
                    committed.extend(frames.feed(piece, now));
                }
            }
            assert!(!frames.pending(), "split {split}");
            assert_eq!(
                text(&committed),
                b"old\x1b[?2026;25hnewmore\x1b[?2026lafter"
            );
            assert!(committed.iter().any(|p| matches!(p, Piece::Apc(_))));
        }
        let mut frames = Frames::default();
        let now = Instant::now();
        assert_eq!(
            text(&frames.feed(Piece::Text(b"\x1b[?2026hhidden".to_vec()), now)),
            b"\x1b[?2026h"
        );
        assert_eq!(
            text(&frames.feed(Piece::Text(b"\x1bc".to_vec()), now)),
            b"hidden\x1bc"
        );
        assert!(!frames.pending());
    }

    #[test]
    fn completed_frame_publishes_before_an_incomplete_next_frame() {
        let mut frames = Frames::default();
        let now = Instant::now();
        let out = frames.feed(
            Piece::Text(b"\x1b[?2026hfirst\x1b[?2026l\x1b[?2026hsecond".to_vec()),
            now,
        );
        assert_eq!(text(&out), b"\x1b[?2026hfirst\x1b[?2026l\x1b[?2026h");
        assert_eq!(text(&frames.expire(now + FRAME_TIMEOUT)), b"second");
        assert_eq!(
            text(&frames.feed(Piece::Text(b"after".to_vec()), now + FRAME_TIMEOUT)),
            b"after"
        );
    }

    #[test]
    fn frame_memory_and_timeout_are_bounded() {
        let mut frames = Frames::default();
        let now = Instant::now();
        frames.feed(Piece::Text(b"\x1b[?2026h".to_vec()), now);
        frames.feed(Piece::Text(vec![b'x'; FRAME_LIMIT]), now);
        assert!(frames.pending());
        let out = frames.feed(Piece::Text(vec![b'y']), now);
        assert_eq!(text(&out).len(), FRAME_LIMIT + 1);
        assert!(!frames.pending());
    }
}
