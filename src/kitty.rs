//! Kitty graphics protocol parsing.

use std::{collections::HashMap, io::Cursor};

use base64::Engine as _;
use rio_vt::crosswords::pos::Column;

use crate::bitmap::BitmapLimits;
use crate::inline::{InlineAnchor, InlineObject, InlineStyle, KittyInlineObject, RasterObject};
use crate::vt::{self, CellColor, VtTerminal};

/// Kitty graphics APC prefix.
pub const KITTY_APC_START: &[u8] = b"\x1b_G";
const ST: &[u8] = b"\x1b\\";
const C1_ST: u8 = 0x9c;

/// Parser state for Kitty graphics sequences.
#[derive(Default)]
pub struct KittyParserState {
    transfer: Option<KittyTransfer>,
    next_object_id: u32,
    limits: BitmapLimits,
}

impl KittyParserState {
    pub(crate) fn with_limits(limits: BitmapLimits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    /// Consumes a Kitty graphics APC sequence.
    pub fn consume_sequence(
        &mut self,
        sequence: &[u8],
        cursor_position: (u16, u16),
    ) -> Option<KittyOperation> {
        if !sequence.starts_with(KITTY_APC_START) {
            return None;
        }

        let content_end = if sequence.ends_with(&[C1_ST]) {
            sequence.len() - 1
        } else if sequence.ends_with(ST) {
            sequence.len() - 2
        } else {
            return None;
        };
        let content = &sequence[KITTY_APC_START.len()..content_end];
        let separator = content.iter().position(|byte| *byte == b';')?;
        let header = std::str::from_utf8(&content[..separator]).ok()?;
        let payload = &content[separator + 1..];

        let mut params = HashMap::new();
        for part in header.split(',').filter(|part| !part.is_empty()) {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            params.insert(key, value);
        }

        let action = params.get("a").copied().unwrap_or("T");
        match action {
            "q" => {
                let image_id = params
                    .get("i")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let quiet = params
                    .get("q")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let format = params
                    .get("f")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(32);
                let width = params
                    .get("s")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let height = params
                    .get("v")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
                let medium = params.get("t").copied().unwrap_or("d");
                let result = decode_query_payload(payload, self.limits).and_then(|payload| {
                    validate_direct_query(format, width, height, medium, &payload, self.limits)
                });
                Some(KittyOperation::Query {
                    image_id,
                    result,
                    quiet,
                })
            }
            "T" | "t" => {
                let starts_new_transfer = self.transfer.is_none()
                    || params.contains_key("a")
                    || params.contains_key("f")
                    || params.contains_key("s")
                    || params.contains_key("v")
                    || params.contains_key("i");
                if starts_new_transfer {
                    let object_id = params
                        .get("i")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(self.next_object_id.max(1));
                    self.next_object_id = self.next_object_id.max(object_id + 1);
                    self.transfer = Some(KittyTransfer {
                        action: action.to_owned(),
                        object_id,
                        format: params
                            .get("f")
                            .and_then(|value| value.parse().ok())
                            .unwrap_or(100),
                        width: params.get("s").and_then(|value| value.parse().ok()),
                        height: params.get("v").and_then(|value| value.parse().ok()),
                        columns: params.get("c").and_then(|value| value.parse().ok()),
                        rows: params.get("r").and_then(|value| value.parse().ok()),
                        uses_placeholders: params.get("U").copied() == Some("1"),
                        anchor_row: cursor_position.0,
                        anchor_col: cursor_position.1,
                        bytes: Vec::new(),
                    });
                }

                let transfer = self.transfer.as_mut()?;
                let chunk = base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .ok()?;
                transfer.bytes.extend_from_slice(&chunk);

                if params.get("m").copied().unwrap_or("0") == "1" {
                    return Some(KittyOperation::Pending);
                }

                let transfer = self.transfer.take()?;
                let image = transfer.finalize()?;
                if transfer.action == "T" {
                    return Some(KittyOperation::TransmitAndPlace {
                        object_id: transfer.object_id,
                        image,
                        anchor: KittyAnchor {
                            row: transfer.anchor_row,
                            col: transfer.anchor_col,
                            columns: transfer.columns.unwrap_or(1),
                            rows: transfer.rows.unwrap_or(1),
                        },
                    });
                }
                Some(KittyOperation::TransmitOnly {
                    object_id: transfer.object_id,
                    image,
                })
            }
            "p" => Some(KittyOperation::PlaceExisting {
                object_id: params.get("i")?.parse().ok()?,
                anchor: KittyAnchor {
                    row: cursor_position.0,
                    col: cursor_position.1,
                    columns: params
                        .get("c")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(1),
                    rows: params
                        .get("r")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(1),
                },
            }),
            "d" => Some(match params.get("i").and_then(|value| value.parse().ok()) {
                Some(object_id) => KittyOperation::Delete {
                    object_id: Some(object_id),
                },
                None => KittyOperation::Delete { object_id: None },
            }),
            _ => Some(KittyOperation::Ignored),
        }
    }
}

/// Decoded Kitty image payload.
#[derive(Default)]
pub struct KittyImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// RGBA image bytes.
    pub rgba: Vec<u8>,
    /// Indicates placeholder mode.
    pub uses_placeholders: bool,
}

impl KittyImage {
    /// Converts the decoded image into an inline object.
    pub fn rasterize(self) -> KittyInlineObject {
        KittyInlineObject {
            raster: RasterObject {
                width: self.width,
                height: self.height,
                rgba: self.rgba,
                handle: None,
            },
            uses_placeholders: self.uses_placeholders,
            plane: None,
        }
    }
}

/// Kitty object anchor.
#[derive(Clone, Copy)]
pub struct KittyAnchor {
    /// Anchor row.
    pub row: u16,
    /// Anchor column.
    pub col: u16,
    /// Object width in cells.
    pub columns: u32,
    /// Object height in cells.
    pub rows: u32,
}

/// Parsed Kitty graphics operation.
pub enum KittyOperation {
    /// Indicates a multipart transfer is still pending.
    Pending,
    /// Indicates the sequence was ignored.
    Ignored,
    /// Capability query result, returned without storing image state.
    Query {
        /// Image identifier echoed in the protocol reply.
        image_id: u32,
        /// Validation result for the probed payload.
        result: Result<(), &'static str>,
        /// Kitty `q` response-suppression level.
        quiet: u8,
    },
    /// Image registration without placement.
    TransmitOnly {
        /// Object identifier.
        object_id: u32,
        /// Decoded image.
        image: KittyImage,
    },
    /// Image registration with placement.
    TransmitAndPlace {
        /// Object identifier.
        object_id: u32,
        /// Decoded image.
        image: KittyImage,
        /// Placement anchor.
        anchor: KittyAnchor,
    },
    /// Placement of a previously registered image.
    PlaceExisting {
        /// Object identifier.
        object_id: u32,
        /// Placement anchor.
        anchor: KittyAnchor,
    },
    /// Image deletion.
    Delete {
        /// Optional object identifier.
        object_id: Option<u32>,
    },
}

fn validate_direct_query(
    format: u32,
    width: u32,
    height: u32,
    medium: &str,
    payload: &[u8],
    limits: BitmapLimits,
) -> Result<(), &'static str> {
    if medium != "d" {
        return Err("unsupported transmission medium");
    }
    if width > limits.max_image_width || height > limits.max_image_height {
        return Err("image dimensions exceed limit");
    }
    if matches!(format, 24 | 32) && (width == 0 || height == 0) {
        return Err("raw image dimensions must be nonzero");
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or("image dimensions exceed limit")?;
    let expected = match format {
        24 => pixels.checked_mul(3),
        32 => pixels.checked_mul(4),
        100 => {
            let decoder_limits = limits.decoder_limits();
            let mut dimension_reader =
                image::ImageReader::with_format(Cursor::new(payload), image::ImageFormat::Png);
            dimension_reader.limits(decoder_limits.clone());
            let (png_width, png_height) = dimension_reader
                .into_dimensions()
                .map_err(|_| "invalid or oversized PNG data")?;
            let decoded_bytes = u64::from(png_width)
                .checked_mul(u64::from(png_height))
                .and_then(|pixels| pixels.checked_mul(4))
                .filter(|bytes| *bytes <= limits.max_bitmap_bytes)
                .ok_or("decoded PNG exceeds limit")?;

            let mut reader =
                image::ImageReader::with_format(Cursor::new(payload), image::ImageFormat::Png);
            reader.limits(decoder_limits);
            let decoded = reader
                .decode()
                .map_err(|_| "invalid or oversized PNG data")?;
            if u64::try_from(decoded.to_rgba8().as_raw().len()).ok() != Some(decoded_bytes) {
                return Err("decoded PNG byte length does not match its dimensions");
            }
            return Ok(());
        }
        _ => return Err("unsupported pixel format"),
    }
    .filter(|bytes| *bytes <= limits.max_bitmap_bytes)
    .ok_or("decoded image exceeds limit")?;
    if payload.len() as u64 != expected {
        return Err("invalid pixel data");
    }
    Ok(())
}

fn decode_query_payload(payload: &[u8], limits: BitmapLimits) -> Result<Vec<u8>, &'static str> {
    let max_encoded_bytes = limits
        .max_bitmap_bytes
        .checked_add(2)
        .map(|bytes| bytes / 3)
        .and_then(|groups| groups.checked_mul(4))
        .unwrap_or(u64::MAX);
    if u64::try_from(payload.len()).unwrap_or(u64::MAX) > max_encoded_bytes {
        return Err("query payload exceeds limit");
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| "invalid pixel data")?;
    if u64::try_from(decoded.len()).unwrap_or(u64::MAX) > limits.max_bitmap_bytes {
        return Err("query payload exceeds limit");
    }
    Ok(decoded)
}

struct KittyTransfer {
    action: String,
    object_id: u32,
    format: u32,
    width: Option<u32>,
    height: Option<u32>,
    columns: Option<u32>,
    rows: Option<u32>,
    uses_placeholders: bool,
    anchor_row: u16,
    anchor_col: u16,
    bytes: Vec<u8>,
}

impl KittyTransfer {
    fn finalize(&self) -> Option<KittyImage> {
        let (width, height, rgba) = match self.format {
            100 => {
                let image =
                    image::load_from_memory_with_format(&self.bytes, image::ImageFormat::Png)
                        .ok()?;
                let rgba = image.to_rgba8();
                (rgba.width(), rgba.height(), rgba.into_raw())
            }
            24 => {
                let width = self.width?;
                let height = self.height?;
                let expected = width as usize * height as usize * 3;
                if self.bytes.len() != expected {
                    return None;
                }
                let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
                for rgb in self.bytes.chunks_exact(3) {
                    rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
                (width, height, rgba)
            }
            32 => {
                let width = self.width?;
                let height = self.height?;
                let expected = width as usize * height as usize * 4;
                if self.bytes.len() != expected {
                    return None;
                }
                (width, height, self.bytes.clone())
            }
            _ => return None,
        };

        Some(KittyImage {
            width,
            height,
            rgba,
            uses_placeholders: self.uses_placeholders,
        })
    }
}

/// Refreshes placeholder-backed Kitty anchors from the terminal grid.
pub fn refresh_kitty_placeholder_anchors(
    objects: &HashMap<u32, InlineObject>,
    anchors: &mut HashMap<u32, InlineAnchor>,
    term: &VtTerminal,
) -> bool {
    let placeholder_ids = objects
        .iter()
        .filter_map(|(object_id, object)| match object {
            InlineObject::KittyImage(object) => object.uses_placeholders.then_some(*object_id),
            InlineObject::RgpObject(_) => None,
        })
        .collect::<Vec<_>>();
    if placeholder_ids.is_empty() {
        return false;
    }
    let placeholder_lookup = placeholder_ids
        .iter()
        .map(|object_id| (object_id & 0x00ff_ffff, *object_id))
        .collect::<HashMap<_, _>>();

    let mut bounds = HashMap::<u32, (u16, u16, u16, u16)>::new();
    let rows = u16::try_from(term.screen_lines()).unwrap_or(u16::MAX);
    let cols = u16::try_from(term.columns()).unwrap_or(u16::MAX);
    let styles = vt::styles(term);
    for row in 0..rows {
        let Some(grid_row) = vt::visible_row(term, row) else {
            continue;
        };
        // rio-vt flags rows holding a U+10EEEE placeholder, so rows without one
        // skip the per-cell scan entirely.
        if !grid_row.kitty_virtual_placeholder {
            continue;
        }
        for col in 0..cols {
            let square = grid_row[Column(usize::from(col))];
            if square.c() != '\u{10EEEE}' {
                continue;
            }
            let (fg, _, _) = vt::cell_attributes(styles, square);
            let CellColor::Rgb(r, g, b) = fg else {
                continue;
            };
            let placeholder_id = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let Some(object_id) = placeholder_lookup.get(&placeholder_id).copied() else {
                continue;
            };
            bounds
                .entry(object_id)
                .and_modify(|(top, left, bottom, right)| {
                    *top = (*top).min(row);
                    *left = (*left).min(col);
                    *bottom = (*bottom).max(row);
                    *right = (*right).max(col);
                })
                .or_insert((row, col, row, col));
        }
    }

    let mut changed = false;
    for object_id in placeholder_ids {
        if let Some((top, left, bottom, right)) = bounds.get(&object_id).copied() {
            let columns = u32::from(right - left + 1);
            let rows = u32::from(bottom - top + 1);
            let new_anchor = InlineAnchor {
                row: top,
                col: left,
                columns,
                rows,
                style: InlineStyle::default(),
            };
            changed |= anchors
                .insert(object_id, new_anchor)
                .is_none_or(|old_anchor| {
                    old_anchor.row != top
                        || old_anchor.col != left
                        || old_anchor.columns != columns
                        || old_anchor.rows != rows
                });
        } else {
            changed |= anchors.remove(&object_id).is_some();
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_query_parses_valid_direct_transfer_probe() {
        let mut state = KittyParserState::default();

        let operation =
            state.consume_sequence(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\", (0, 0));

        assert!(matches!(
            operation,
            Some(KittyOperation::Query {
                image_id: 31,
                result: Ok(()),
                quiet: 0,
            })
        ));
    }

    #[test]
    fn kitty_query_preserves_quiet_level() {
        let mut state = KittyParserState::default();

        let operation =
            state.consume_sequence(b"\x1b_Gi=9,s=2,v=2,a=q,t=d,f=24,q=2;AAAA\x1b\\", (0, 0));

        assert!(matches!(
            operation,
            Some(KittyOperation::Query {
                image_id: 9,
                result: Err("invalid pixel data"),
                quiet: 2,
            })
        ));
    }

    #[test]
    fn kitty_query_defaults_to_rgba() {
        let mut state = KittyParserState::default();

        let operation =
            state.consume_sequence(b"\x1b_Gi=10,s=1,v=1,a=q,t=d;AAAAAA==\x1b\\", (0, 0));

        assert!(matches!(
            operation,
            Some(KittyOperation::Query {
                image_id: 10,
                result: Ok(()),
                quiet: 0,
            })
        ));
    }

    #[test]
    fn kitty_raw_query_rejects_zero_dimensions() {
        let mut state = KittyParserState::default();

        let operation = state.consume_sequence(b"\x1b_Gi=13,a=q,t=d,f=32;\x1b\\", (0, 0));

        assert!(matches!(
            operation,
            Some(KittyOperation::Query {
                result: Err("raw image dimensions must be nonzero"),
                ..
            })
        ));
    }

    #[test]
    fn kitty_query_rejects_payload_before_decoding_past_limit() {
        let mut state = KittyParserState::with_limits(BitmapLimits {
            max_bitmap_bytes: 2,
            ..BitmapLimits::default()
        });

        let operation =
            state.consume_sequence(b"\x1b_Gi=11,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\", (0, 0));

        assert!(matches!(
            operation,
            Some(KittyOperation::Query {
                result: Err("query payload exceeds limit"),
                ..
            })
        ));
    }

    #[test]
    fn kitty_png_query_obeys_decoder_limits() {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode PNG");
        let payload = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
        let sequence = format!("\x1b_Gi=12,a=q,t=d,f=100;{payload}\x1b\\");
        let mut state = KittyParserState::with_limits(BitmapLimits {
            max_image_width: 1,
            max_image_height: 1,
            ..BitmapLimits::default()
        });

        let operation = state.consume_sequence(sequence.as_bytes(), (0, 0));

        assert!(matches!(
            operation,
            Some(KittyOperation::Query {
                result: Err("invalid or oversized PNG data"),
                ..
            })
        ));
    }
}
