//! Kitty graphics protocol parsing.

use std::collections::HashMap;

use base64::Engine as _;
use image::ImageDecoder;
use std::io::Cursor;

const TRANSFER_LIMIT: usize = 32 * 1024 * 1024;
const RASTER_LIMIT: usize = 64 * 1024 * 1024;

use crate::inline::{InlineAnchor, InlineObject, InlineStyle, KittyInlineObject, RasterObject};
use fux_vt::Color;

use crate::screen::{KITTY_PLACEHOLDER, ScreenView};

/// Kitty graphics APC prefix.
pub const KITTY_APC_START: &[u8] = b"\x1b_G";
const ST: &[u8] = b"\x1b\\";
const C1_ST: u8 = 0x9c;

/// Parser state for Kitty graphics sequences.
#[derive(Default)]
pub struct KittyParserState {
    transfer: Transfer,
    next_object_id: u32,
}

impl KittyParserState {
    pub(crate) fn abort_transfer(&mut self) {
        self.transfer = Transfer::Discarding;
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
            "T" | "t" => {
                let starts_new_transfer = self.transfer.is_none()
                    || params.contains_key("a")
                    || params.contains_key("f")
                    || params.contains_key("s")
                    || params.contains_key("v")
                    || params.contains_key("i");
                let more = params.get("m").copied().unwrap_or("0") == "1";
                if matches!(self.transfer, Transfer::Discarding) && !starts_new_transfer {
                    if !more {
                        self.transfer = Transfer::Idle;
                    }
                    return Some(KittyOperation::Ignored);
                }
                if starts_new_transfer {
                    let object_id = params
                        .get("i")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(self.next_object_id.max(1));
                    self.next_object_id = self.next_object_id.max(object_id.wrapping_add(1).max(1));
                    self.transfer = Transfer::Receiving(KittyTransfer {
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
                let chunk = match base64::engine::general_purpose::STANDARD.decode(payload) {
                    Ok(chunk)
                        if transfer.bytes.len().saturating_add(chunk.len()) <= TRANSFER_LIMIT =>
                    {
                        chunk
                    }
                    _ => {
                        self.transfer = if more {
                            Transfer::Discarding
                        } else {
                            Transfer::Idle
                        };
                        bevy::log::warn!(
                            "Kitty transfer rejected: invalid base64 or more than 32 MiB"
                        );
                        return Some(KittyOperation::Ignored);
                    }
                };
                transfer.bytes.extend_from_slice(&chunk);
                if more {
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

#[derive(Default)]
enum Transfer {
    #[default]
    Idle,
    Receiving(KittyTransfer),
    Discarding,
}
impl Transfer {
    fn is_none(&self) -> bool {
        matches!(self, Self::Idle)
    }
    fn as_mut(&mut self) -> Option<&mut KittyTransfer> {
        match self {
            Self::Receiving(t) => Some(t),
            _ => None,
        }
    }
    fn take(&mut self) -> Option<KittyTransfer> {
        match std::mem::take(self) {
            Self::Receiving(t) => Some(t),
            _ => None,
        }
    }
}

fn rgba_len(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return None;
    }
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
        .filter(|bytes| *bytes <= RASTER_LIMIT)
}

impl KittyTransfer {
    fn finalize(&self) -> Option<KittyImage> {
        let (width, height, rgba) = match self.format {
            100 => {
                let mut reader = image::ImageReader::with_format(
                    Cursor::new(&self.bytes),
                    image::ImageFormat::Png,
                );
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(8192);
                limits.max_image_height = Some(8192);
                limits.max_alloc = Some(RASTER_LIMIT as u64);
                reader.limits(limits);
                let decoder = reader.into_decoder().ok()?;
                let (width, height) = decoder.dimensions();
                rgba_len(width, height)?;
                let image = image::DynamicImage::from_decoder(decoder).ok()?;
                let rgba = image.to_rgba8();
                (rgba.width(), rgba.height(), rgba.into_raw())
            }
            24 => {
                let width = self.width?;
                let height = self.height?;
                let rgba_bytes = rgba_len(width, height)?;
                let expected = rgba_bytes / 4 * 3;
                if self.bytes.len() != expected {
                    return None;
                }
                let mut rgba = Vec::with_capacity(rgba_bytes);
                for rgb in self.bytes.as_chunks::<3>().0 {
                    rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
                (width, height, rgba)
            }
            32 => {
                let width = self.width?;
                let height = self.height?;
                let expected = rgba_len(width, height)?;
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
    screen: ScreenView<'_>,
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
    let (rows, cols): (u16, u16) = screen.size().into();
    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            // Placeholders may carry combining diacritics that encode the
            // image row and column, so match on the base character.
            if !cell.contents().starts_with(KITTY_PLACEHOLDER) {
                continue;
            }
            let Color::Rgb(fux_vt::Rgb { r, g, b }) = cell.fgcolor() else {
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
                row: i32::from(top),
                col: left,
                columns,
                rows,
                style: InlineStyle::default(),
            };
            changed |= anchors
                .insert(object_id, new_anchor)
                .is_none_or(|old_anchor| {
                    old_anchor.row != i32::from(top)
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
mod transfer_tests {
    use super::*;

    #[test]
    fn chunked_pixels_survive_and_invalid_chunks_require_a_new_transfer() {
        let mut parser = KittyParserState::default();
        assert!(matches!(
            parser.consume_sequence(b"\x1b_Ga=T,f=32,s=1,v=1,i=9,m=1;/wA=\x1b\\", (0, 0)),
            Some(KittyOperation::Pending)
        ));
        let Some(KittyOperation::TransmitAndPlace { image, .. }) =
            parser.consume_sequence(b"\x1b_Gm=0;AP8=\x1b\\", (0, 0))
        else {
            panic!("complete image");
        };
        assert_eq!(image.rgba, [255, 0, 0, 255]);
        parser.consume_sequence(b"\x1b_Ga=T,m=1;not-base64!\x1b\\", (0, 0));
        assert!(matches!(parser.transfer, Transfer::Discarding));
        assert!(matches!(
            parser.consume_sequence(b"\x1b_Gm=0;AP8=\x1b\\", (0, 0)),
            Some(KittyOperation::Ignored)
        ));
        assert!(matches!(parser.transfer, Transfer::Idle));
    }

    #[test]
    fn graphics_size_and_transfer_limits_prevent_growth_before_decode() {
        let mut parser = KittyParserState::default();
        parser.consume_sequence(b"\x1b_Ga=T,f=32,s=1,v=1,m=1;/wA=\x1b\\", (0, 0));
        parser
            .transfer
            .as_mut()
            .expect("pending transfer")
            .bytes
            .resize(TRANSFER_LIMIT, 0);
        assert!(matches!(
            parser.consume_sequence(b"\x1b_Gm=1;AA==\x1b\\", (0, 0)),
            Some(KittyOperation::Ignored)
        ));
        assert!(matches!(parser.transfer, Transfer::Discarding));
        assert!(rgba_len(u32::MAX, u32::MAX).is_none());
        assert!(rgba_len(8192, 8192).is_none());
        assert_eq!(rgba_len(4096, 4096), Some(RASTER_LIMIT));
        // User IDs may reach u32::MAX without overflowing the allocator.
        parser.consume_sequence(
            b"\x1b_Ga=T,f=32,s=1,v=1,i=4294967295;/wAA/w==\x1b\\",
            (0, 0),
        );
    }
}
