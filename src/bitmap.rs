//! Ratty Bitmap Surface protocol parsing.

use std::{collections::HashMap, fmt};

use base64::Engine as _;
use bevy::prelude::{Handle, Image};

/// Ratty Bitmap Surface APC prefix.
pub const BITMAP_APC_START: &[u8] = b"\x1b_ratty;i;";
const ST: &[u8] = b"\x1b\\";
const C1_ST: u8 = 0x9c;
const SUPPORT_REPLY: &[u8] = b"\x1b_ratty;i;s;v=1;fmt=png;frame=rgba8;payload=1;chunk=1;placement=1;crop=1;fit=contain|cover|fill;filter=nearest|linear;opacity=1\x1b\\";
pub(crate) const MAX_BITMAP_CHUNK_DECODED_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const BITMAP_APC_HEADER_ALLOWANCE: usize = 4 * 1024;
const MAX_BITMAP_CHUNK_BASE64_BYTES: usize = MAX_BITMAP_CHUNK_DECODED_BYTES.div_ceil(3) * 4;
pub(crate) const MAX_BITMAP_APC_BYTES: usize =
    BITMAP_APC_START.len() + BITMAP_APC_HEADER_ALLOWANCE + MAX_BITMAP_CHUNK_BASE64_BYTES + ST.len();
const MAX_REGISTRATION_BYTES: usize = 64 * 1024 * 1024;
const CHUNK_PAYLOAD_TOO_LARGE: &str = "bitmap APC chunk payload exceeds 64 MiB";
const HEADER_TOO_LARGE: &str = "bitmap APC header exceeds 4 KiB";

/// How source pixels are fitted into a placement's destination rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapFit {
    /// Preserve aspect ratio and letterbox the unused destination area.
    Contain,
    /// Preserve aspect ratio and crop symmetrically to fill the destination.
    Cover,
    /// Stretch the source to fill the destination.
    Fill,
}

/// Texture filtering for a bitmap placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapFilter {
    /// Select the nearest source pixel.
    Nearest,
    /// Interpolate between neighboring source pixels.
    Linear,
}

/// A rectangle in source-pixel coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRect {
    /// Horizontal offset from the bitmap's left edge.
    pub x: u32,
    /// Vertical offset from the bitmap's top edge.
    pub y: u32,
    /// Source width in pixels.
    pub width: u32,
    /// Source height in pixels.
    pub height: u32,
}

/// One decoded chunk of a bitmap registration transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitmapRegisterChunk {
    /// Bitmap identifier.
    pub bitmap_id: u32,
    /// Registration format metadata, present on the first chunk.
    pub format: Option<String>,
    /// Registration source metadata, present on the first chunk.
    pub source: Option<String>,
    /// Optional diagnostic payload name.
    pub name: Option<String>,
    /// Whether additional chunks follow.
    pub more: bool,
    /// Decoded payload bytes for this chunk.
    pub data: Vec<u8>,
}

/// A complete bitmap placement request.
#[derive(Clone, Debug, PartialEq)]
pub struct BitmapPlacement {
    /// Bitmap identifier.
    pub bitmap_id: u32,
    /// Globally unique placement identifier.
    pub placement_id: u32,
    /// Destination row in terminal cells.
    pub row: u16,
    /// Destination column in terminal cells.
    pub col: u16,
    /// Destination width in terminal cells.
    pub columns: u32,
    /// Destination height in terminal cells.
    pub rows: u32,
    /// Optional source-pixel crop.
    pub source: Option<SourceRect>,
    /// Fit mode.
    pub fit: BitmapFit,
    /// Filtering mode.
    pub filter: BitmapFilter,
    /// Clamped placement opacity.
    pub opacity: f32,
}

/// Transactional changes to an existing bitmap placement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BitmapPlacementUpdate {
    /// Optional destination row.
    pub row: Option<u16>,
    /// Optional destination column.
    pub col: Option<u16>,
    /// Optional destination width, paired with `rows`.
    pub columns: Option<u32>,
    /// Optional destination height, paired with `columns`.
    pub rows: Option<u32>,
    /// Optional complete source-pixel crop.
    pub source: Option<SourceRect>,
    /// Optional fit mode.
    pub fit: Option<BitmapFit>,
    /// Optional filtering mode.
    pub filter: Option<BitmapFilter>,
    /// Optional clamped opacity.
    pub opacity: Option<f32>,
}

/// One decoded chunk of a sequenced RGBA8 frame transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitmapFrameChunk {
    /// Bitmap identifier.
    pub bitmap_id: u32,
    /// Per-bitmap frame sequence number.
    pub sequence: u32,
    /// Frame format metadata, present on the first chunk.
    pub format: Option<String>,
    /// Frame width metadata, present on the first chunk.
    pub width: Option<u32>,
    /// Frame height metadata, present on the first chunk.
    pub height: Option<u32>,
    /// Whether additional chunks follow.
    pub more: bool,
    /// Decoded RGBA8 payload bytes for this chunk.
    pub data: Vec<u8>,
}

/// A parsed Ratty Bitmap Surface operation.
#[derive(Clone, Debug, PartialEq)]
pub enum BitmapOperation {
    /// Query protocol support.
    SupportQuery,
    /// Register a PNG bitmap transfer chunk.
    Register(BitmapRegisterChunk),
    /// Create a placement.
    Place(BitmapPlacement),
    /// Update an existing placement.
    Update {
        /// Placement identifier.
        placement_id: u32,
        /// Fields to update transactionally.
        update: BitmapPlacementUpdate,
    },
    /// Replace bitmap pixels with a frame transfer chunk.
    Frame(BitmapFrameChunk),
    /// Delete one placement.
    DeletePlacement(u32),
    /// Delete one bitmap and its placements.
    DeleteBitmap(u32),
    /// Consume an unknown protocol verb without mutation.
    Ignored,
}

/// An error in a command within the Ratty Bitmap Surface namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitmapProtocolError {
    message: &'static str,
    cleanup: Option<BitmapErrorCleanup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BitmapErrorCleanup {
    DiscardPendingRegistration {
        bitmap_id: u32,
    },
    DiscardPendingFrame {
        bitmap_id: u32,
        sequence: u32,
        format: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
    },
}

impl BitmapProtocolError {
    fn new(message: &'static str) -> Self {
        Self {
            message,
            cleanup: None,
        }
    }

    fn frame_payload(
        message: &'static str,
        bitmap_id: u32,
        sequence: u32,
        format: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Self {
        Self {
            message,
            cleanup: Some(BitmapErrorCleanup::DiscardPendingFrame {
                bitmap_id,
                sequence,
                format,
                width,
                height,
            }),
        }
    }

    fn registration_payload(message: &'static str, bitmap_id: u32) -> Self {
        Self {
            message,
            cleanup: Some(BitmapErrorCleanup::DiscardPendingRegistration { bitmap_id }),
        }
    }
}

impl fmt::Display for BitmapProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for BitmapProtocolError {}

type Fields<'a> = HashMap<&'a str, &'a str>;

/// Consumes a complete Ratty Bitmap Surface APC sequence.
pub fn consume_sequence(sequence: &[u8]) -> Option<Result<BitmapOperation, BitmapProtocolError>> {
    if !sequence.starts_with(BITMAP_APC_START) {
        return None;
    }

    Some(parse_sequence(sequence))
}

/// Returns the exact v1 support-discovery response.
pub fn support_reply() -> Vec<u8> {
    SUPPORT_REPLY.to_vec()
}

fn parse_sequence(sequence: &[u8]) -> Result<BitmapOperation, BitmapProtocolError> {
    parse_sequence_with_payload_limit(sequence, MAX_BITMAP_CHUNK_DECODED_BYTES)
}

fn parse_sequence_with_payload_limit(
    sequence: &[u8],
    payload_limit: usize,
) -> Result<BitmapOperation, BitmapProtocolError> {
    parse_sequence_with_limits(sequence, payload_limit, BITMAP_APC_HEADER_ALLOWANCE)
}

fn parse_sequence_with_limits(
    sequence: &[u8],
    payload_limit: usize,
    header_limit: usize,
) -> Result<BitmapOperation, BitmapProtocolError> {
    let content_end = if sequence.ends_with(&[C1_ST]) {
        sequence.len() - 1
    } else if sequence.ends_with(ST) {
        sequence.len() - ST.len()
    } else {
        return Err(BitmapProtocolError::new("invalid bitmap APC terminator"));
    };
    let content = std::str::from_utf8(&sequence[BITMAP_APC_START.len()..content_end])
        .map_err(|_| BitmapProtocolError::new("bitmap APC is not valid UTF-8"))?;
    if bitmap_header_extent(content) > header_limit {
        return Err(BitmapProtocolError::new(HEADER_TOO_LARGE));
    }
    let mut parts: Vec<_> = content.split(';').collect();
    let verb = parts
        .first()
        .copied()
        .ok_or_else(|| BitmapProtocolError::new("missing bitmap verb"))?;
    if verb.is_empty() {
        return Err(BitmapProtocolError::new("missing bitmap verb"));
    }
    parts.remove(0);

    match verb {
        "s" => parse_support(&parts),
        "r" => parse_register(&parts, payload_limit),
        "p" => parse_place(&parts),
        "u" => parse_update(&parts),
        "f" => parse_frame(&parts, payload_limit),
        "d" => parse_delete(&parts),
        _ => Ok(BitmapOperation::Ignored),
    }
}

fn bitmap_header_extent(content: &str) -> usize {
    if content.starts_with("r;") || content.starts_with("f;") {
        content
            .rfind(';')
            .map_or(content.len(), |separator| separator + 1)
    } else {
        content.len()
    }
}

fn parse_support(parts: &[&str]) -> Result<BitmapOperation, BitmapProtocolError> {
    if parts.is_empty() {
        Ok(BitmapOperation::SupportQuery)
    } else {
        Err(BitmapProtocolError::new(
            "support query does not accept fields",
        ))
    }
}

fn parse_register(
    parts: &[&str],
    payload_limit: usize,
) -> Result<BitmapOperation, BitmapProtocolError> {
    let (payload, header) = split_payload(parts)?;
    let fields = parse_fields(header, &["id", "fmt", "source", "more", "name"])?;
    let format = optional_string(&fields, "fmt");
    let source = optional_string(&fields, "source");
    if format.is_some() != source.is_some() {
        return Err(BitmapProtocolError::new(
            "registration format and source must be provided together",
        ));
    }
    if format.as_deref().is_some_and(|value| value != "png") {
        return Err(BitmapProtocolError::new("unsupported bitmap format"));
    }
    if source.as_deref().is_some_and(|value| value != "payload") {
        return Err(BitmapProtocolError::new(
            "unsupported bitmap registration source",
        ));
    }

    let bitmap_id = required_u32(&fields, "id")?;
    let data = decode_payload(payload, payload_limit).map_err(|error| {
        if error.message == CHUNK_PAYLOAD_TOO_LARGE {
            BitmapProtocolError::registration_payload(error.message, bitmap_id)
        } else {
            error
        }
    })?;

    Ok(BitmapOperation::Register(BitmapRegisterChunk {
        bitmap_id,
        format,
        source,
        name: optional_string(&fields, "name"),
        more: required_bool(&fields, "more")?,
        data,
    }))
}

fn parse_place(parts: &[&str]) -> Result<BitmapOperation, BitmapProtocolError> {
    let fields = parse_fields(
        parts,
        &[
            "id", "pid", "row", "col", "w", "h", "src_x", "src_y", "src_w", "src_h", "fit",
            "filter", "opacity",
        ],
    )?;
    let columns = required_nonzero_u32(&fields, "w")?;
    let rows = required_nonzero_u32(&fields, "h")?;

    Ok(BitmapOperation::Place(BitmapPlacement {
        bitmap_id: required_u32(&fields, "id")?,
        placement_id: required_u32(&fields, "pid")?,
        row: required_u16(&fields, "row")?,
        col: required_u16(&fields, "col")?,
        columns,
        rows,
        source: parse_source(&fields)?,
        fit: parse_fit(fields.get("fit").copied())?.unwrap_or(BitmapFit::Contain),
        filter: parse_filter(fields.get("filter").copied())?.unwrap_or(BitmapFilter::Linear),
        opacity: parse_opacity(fields.get("opacity").copied())?.unwrap_or(1.0),
    }))
}

fn parse_update(parts: &[&str]) -> Result<BitmapOperation, BitmapProtocolError> {
    let fields = parse_fields(
        parts,
        &[
            "pid", "row", "col", "w", "h", "src_x", "src_y", "src_w", "src_h", "fit", "filter",
            "opacity",
        ],
    )?;
    let placement_id = required_u32(&fields, "pid")?;
    let columns = optional_nonzero_u32(&fields, "w")?;
    let rows = optional_nonzero_u32(&fields, "h")?;
    if columns.is_some() != rows.is_some() {
        return Err(BitmapProtocolError::new(
            "update width and height must be provided together",
        ));
    }
    let update = BitmapPlacementUpdate {
        row: optional_u16(&fields, "row")?,
        col: optional_u16(&fields, "col")?,
        columns,
        rows,
        source: parse_source(&fields)?,
        fit: parse_fit(fields.get("fit").copied())?,
        filter: parse_filter(fields.get("filter").copied())?,
        opacity: parse_opacity(fields.get("opacity").copied())?,
    };
    if update == BitmapPlacementUpdate::default() {
        return Err(BitmapProtocolError::new(
            "placement update contains no mutable fields",
        ));
    }

    Ok(BitmapOperation::Update {
        placement_id,
        update,
    })
}

fn parse_frame(
    parts: &[&str],
    payload_limit: usize,
) -> Result<BitmapOperation, BitmapProtocolError> {
    let (payload, header) = split_payload(parts)?;
    let fields = parse_fields(header, &["id", "seq", "fmt", "w", "h", "more"])?;
    let format = optional_string(&fields, "fmt");
    let width = optional_nonzero_u32(&fields, "w")?;
    let height = optional_nonzero_u32(&fields, "h")?;
    let metadata_count = usize::from(format.is_some())
        + usize::from(width.is_some())
        + usize::from(height.is_some());
    if metadata_count != 0 && metadata_count != 3 {
        return Err(BitmapProtocolError::new(
            "frame format and dimensions must be provided together",
        ));
    }
    if format.as_deref().is_some_and(|value| value != "rgba8") {
        return Err(BitmapProtocolError::new("unsupported bitmap frame format"));
    }
    let bitmap_id = required_u32(&fields, "id")?;
    let sequence = required_u32(&fields, "seq")?;
    let more = required_bool(&fields, "more")?;
    let data = decode_payload(payload, payload_limit).map_err(|error| {
        BitmapProtocolError::frame_payload(
            error.message,
            bitmap_id,
            sequence,
            format.clone(),
            width,
            height,
        )
    })?;

    Ok(BitmapOperation::Frame(BitmapFrameChunk {
        bitmap_id,
        sequence,
        format,
        width,
        height,
        more,
        data,
    }))
}

fn parse_delete(parts: &[&str]) -> Result<BitmapOperation, BitmapProtocolError> {
    let fields = parse_fields(parts, &["id", "pid"])?;
    match (fields.get("id"), fields.get("pid")) {
        (Some(id), None) => Ok(BitmapOperation::DeleteBitmap(parse_u32(id)?)),
        (None, Some(placement_id)) => {
            Ok(BitmapOperation::DeletePlacement(parse_u32(placement_id)?))
        }
        _ => Err(BitmapProtocolError::new(
            "delete requires exactly one bitmap or placement ID",
        )),
    }
}

fn split_payload<'a>(
    parts: &'a [&'a str],
) -> Result<(&'a str, &'a [&'a str]), BitmapProtocolError> {
    let (payload, header) = parts
        .split_last()
        .ok_or_else(|| BitmapProtocolError::new("missing bitmap payload"))?;
    if payload.is_empty() {
        return Err(BitmapProtocolError::new("missing bitmap payload"));
    }
    Ok((payload, header))
}

fn parse_fields<'a>(
    parts: &'a [&'a str],
    allowed: &[&str],
) -> Result<Fields<'a>, BitmapProtocolError> {
    let mut fields = HashMap::new();
    for part in parts {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| BitmapProtocolError::new("malformed bitmap field"))?;
        if !allowed.contains(&key) {
            return Err(BitmapProtocolError::new("unknown bitmap field"));
        }
        if fields.insert(key, value).is_some() {
            return Err(BitmapProtocolError::new("duplicate bitmap field"));
        }
    }
    Ok(fields)
}

fn required_u32(fields: &Fields<'_>, key: &str) -> Result<u32, BitmapProtocolError> {
    fields
        .get(key)
        .ok_or_else(|| BitmapProtocolError::new("missing required bitmap field"))
        .and_then(|value| parse_u32(value))
}

fn parse_u32(value: &str) -> Result<u32, BitmapProtocolError> {
    value
        .parse()
        .map_err(|_| BitmapProtocolError::new("invalid unsigned bitmap integer"))
}

fn required_nonzero_u32(fields: &Fields<'_>, key: &str) -> Result<u32, BitmapProtocolError> {
    let value = required_u32(fields, key)?;
    if value == 0 {
        Err(BitmapProtocolError::new(
            "bitmap dimensions must be nonzero",
        ))
    } else {
        Ok(value)
    }
}

fn optional_nonzero_u32(
    fields: &Fields<'_>,
    key: &str,
) -> Result<Option<u32>, BitmapProtocolError> {
    fields
        .get(key)
        .map(|value| {
            let value = parse_u32(value)?;
            if value == 0 {
                Err(BitmapProtocolError::new(
                    "bitmap dimensions must be nonzero",
                ))
            } else {
                Ok(value)
            }
        })
        .transpose()
}

fn required_u16(fields: &Fields<'_>, key: &str) -> Result<u16, BitmapProtocolError> {
    fields
        .get(key)
        .ok_or_else(|| BitmapProtocolError::new("missing required bitmap field"))?
        .parse()
        .map_err(|_| BitmapProtocolError::new("invalid terminal-cell coordinate"))
}

fn optional_u16(fields: &Fields<'_>, key: &str) -> Result<Option<u16>, BitmapProtocolError> {
    fields
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|_| BitmapProtocolError::new("invalid terminal-cell coordinate"))
        })
        .transpose()
}

fn required_bool(fields: &Fields<'_>, key: &str) -> Result<bool, BitmapProtocolError> {
    match fields.get(key).copied() {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(_) => Err(BitmapProtocolError::new("invalid bitmap boolean")),
        None => Err(BitmapProtocolError::new("missing required bitmap field")),
    }
}

fn optional_string(fields: &Fields<'_>, key: &str) -> Option<String> {
    fields.get(key).map(|value| (*value).to_owned())
}

fn parse_source(fields: &Fields<'_>) -> Result<Option<SourceRect>, BitmapProtocolError> {
    let values = ["src_x", "src_y", "src_w", "src_h"].map(|key| fields.get(key).copied());
    let [x, y, width, height] = match values {
        [None, None, None, None] => return Ok(None),
        [Some(x), Some(y), Some(width), Some(height)] => [x, y, width, height],
        _ => {
            return Err(BitmapProtocolError::new(
                "source rectangle requires all four fields",
            ));
        }
    };
    let width = parse_u32(width)?;
    let height = parse_u32(height)?;
    if width == 0 || height == 0 {
        return Err(BitmapProtocolError::new(
            "source dimensions must be nonzero",
        ));
    }
    Ok(Some(SourceRect {
        x: parse_u32(x)?,
        y: parse_u32(y)?,
        width,
        height,
    }))
}

fn parse_fit(value: Option<&str>) -> Result<Option<BitmapFit>, BitmapProtocolError> {
    match value {
        None => Ok(None),
        Some("contain") => Ok(Some(BitmapFit::Contain)),
        Some("cover") => Ok(Some(BitmapFit::Cover)),
        Some("fill") => Ok(Some(BitmapFit::Fill)),
        Some(_) => Err(BitmapProtocolError::new("unsupported bitmap fit mode")),
    }
}

fn parse_filter(value: Option<&str>) -> Result<Option<BitmapFilter>, BitmapProtocolError> {
    match value {
        None => Ok(None),
        Some("nearest") => Ok(Some(BitmapFilter::Nearest)),
        Some("linear") => Ok(Some(BitmapFilter::Linear)),
        Some(_) => Err(BitmapProtocolError::new("unsupported bitmap filter mode")),
    }
}

fn parse_opacity(value: Option<&str>) -> Result<Option<f32>, BitmapProtocolError> {
    value
        .map(|value| {
            let opacity: f32 = value
                .parse()
                .map_err(|_| BitmapProtocolError::new("invalid bitmap opacity"))?;
            if !opacity.is_finite() {
                return Err(BitmapProtocolError::new("bitmap opacity must be finite"));
            }
            Ok(opacity.clamp(0.0, 1.0))
        })
        .transpose()
}

fn decode_payload(payload: &str, payload_limit: usize) -> Result<Vec<u8>, BitmapProtocolError> {
    let estimated_len = estimated_decoded_payload_len(payload)
        .ok_or_else(|| BitmapProtocolError::new("bitmap payload length overflow"))?;
    if estimated_len > payload_limit {
        return Err(BitmapProtocolError::new(CHUNK_PAYLOAD_TOO_LARGE));
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|_| BitmapProtocolError::new("invalid bitmap payload base64"))
}

fn estimated_decoded_payload_len(payload: &str) -> Option<usize> {
    let len = payload.len();
    let remainder_bytes = match len % 4 {
        0 => 0,
        2 => 1,
        3 => 2,
        _ => 3,
    };
    let padding = if len.is_multiple_of(4) {
        payload
            .as_bytes()
            .iter()
            .rev()
            .take(2)
            .take_while(|byte| **byte == b'=')
            .count()
    } else {
        0
    };
    (len / 4)
        .checked_mul(3)?
        .checked_add(remainder_bytes)?
        .checked_sub(padding)
}

/// A decoded bitmap and its eventual stable Bevy image handle.
pub struct RegisteredBitmap {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    handle: Option<Handle<Image>>,
}

impl RegisteredBitmap {
    /// Returns the bitmap width in pixels.
    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    /// Returns the bitmap height in pixels.
    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    /// Returns the stable Bevy image handle after the renderer uploads the bitmap.
    pub(crate) fn handle(&self) -> Option<&Handle<Image>> {
        self.handle.as_ref()
    }

    /// Moves pixels that have not yet been synchronized into the Bevy image asset.
    pub(crate) fn take_pending_rgba(&mut self) -> Option<Vec<u8>> {
        (!self.rgba.is_empty()).then(|| std::mem::take(&mut self.rgba))
    }

    /// Records the stable Bevy image handle created by the renderer.
    pub(crate) fn set_handle(&mut self, handle: Handle<Image>) {
        debug_assert!(
            self.handle
                .as_ref()
                .is_none_or(|current| current == &handle)
        );
        self.handle = Some(handle);
    }
}

/// The validated state of one independently addressable bitmap placement.
#[derive(Clone, Debug, PartialEq)]
pub struct BitmapPlacementState {
    generation: u64,
    bitmap_id: u32,
    row: u16,
    col: u16,
    columns: u32,
    rows: u32,
    source: Option<SourceRect>,
    fit: BitmapFit,
    filter: BitmapFilter,
    opacity: f32,
}

impl BitmapPlacementState {
    /// Returns this placement lifetime's monotonically increasing generation.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the registered bitmap used by this placement.
    pub(crate) fn bitmap_id(&self) -> u32 {
        self.bitmap_id
    }

    /// Returns the placement's terminal row.
    pub(crate) fn row(&self) -> u16 {
        self.row
    }

    /// Returns the placement's terminal column.
    pub(crate) fn col(&self) -> u16 {
        self.col
    }

    /// Returns the placement width in terminal columns.
    pub(crate) fn columns(&self) -> u32 {
        self.columns
    }

    /// Returns the placement height in terminal rows.
    pub(crate) fn rows(&self) -> u32 {
        self.rows
    }

    /// Returns the validated source crop, if present.
    pub(crate) fn source(&self) -> Option<SourceRect> {
        self.source
    }

    /// Returns the placement fit mode.
    pub(crate) fn fit(&self) -> BitmapFit {
        self.fit
    }

    /// Returns the placement filtering mode.
    pub(crate) fn filter(&self) -> BitmapFilter {
        self.filter
    }

    /// Returns the clamped placement opacity.
    pub(crate) fn opacity(&self) -> f32 {
        self.opacity
    }
}

struct PendingBitmapTransfer {
    format: String,
    source: String,
    name: Option<String>,
    data: Vec<u8>,
}

struct PendingBitmapFrame {
    sequence: u32,
    format: String,
    width: u32,
    height: u32,
    expected_len: usize,
    data: Vec<u8>,
}

/// In-memory lifecycle state for registered bitmaps, frames, and placements.
#[derive(Default)]
pub struct BitmapSurfaceState {
    pending_registrations: HashMap<u32, PendingBitmapTransfer>,
    pending_frames: HashMap<u32, PendingBitmapFrame>,
    bitmaps: HashMap<u32, RegisteredBitmap>,
    placements: HashMap<u32, BitmapPlacementState>,
    latest_frame_sequences: HashMap<u32, u32>,
    next_placement_generation: u64,
    dirty: bool,
}

impl BitmapSurfaceState {
    /// Parses and applies one complete bitmap APC sequence, including parser-directed cleanup.
    pub(crate) fn consume_and_apply(
        &mut self,
        sequence: &[u8],
    ) -> Option<Result<Option<Vec<u8>>, BitmapProtocolError>> {
        let parsed = consume_sequence(sequence)?;
        Some(match parsed {
            Ok(operation) => self.apply(operation),
            Err(error) => {
                self.apply_error_cleanup(&error);
                Err(error)
            }
        })
    }

    /// Applies one parsed bitmap protocol operation transactionally.
    pub fn apply(
        &mut self,
        operation: BitmapOperation,
    ) -> Result<Option<Vec<u8>>, BitmapProtocolError> {
        match operation {
            BitmapOperation::SupportQuery => Ok(Some(support_reply())),
            BitmapOperation::Register(chunk) => {
                self.apply_registration(chunk)?;
                Ok(None)
            }
            BitmapOperation::Place(placement) => {
                self.apply_placement(placement)?;
                Ok(None)
            }
            BitmapOperation::Update {
                placement_id,
                update,
            } => {
                self.apply_placement_update(placement_id, update)?;
                Ok(None)
            }
            BitmapOperation::Frame(chunk) => {
                self.apply_frame(chunk)?;
                Ok(None)
            }
            BitmapOperation::DeletePlacement(placement_id) => {
                if self.placements.remove(&placement_id).is_some() {
                    self.dirty = true;
                }
                Ok(None)
            }
            BitmapOperation::DeleteBitmap(bitmap_id) => {
                self.delete_bitmap(bitmap_id);
                Ok(None)
            }
            BitmapOperation::Ignored => Ok(None),
        }
    }

    /// Returns a registered bitmap without allowing map mutation.
    pub(crate) fn bitmap(&self, bitmap_id: u32) -> Option<&RegisteredBitmap> {
        self.bitmaps.get(&bitmap_id)
    }

    /// Returns a registered bitmap for renderer-owned upload bookkeeping.
    pub(crate) fn bitmap_mut(&mut self, bitmap_id: u32) -> Option<&mut RegisteredBitmap> {
        self.bitmaps.get_mut(&bitmap_id)
    }

    /// Iterates registered bitmaps without exposing mutable map access.
    pub(crate) fn bitmaps(&self) -> impl Iterator<Item = (&u32, &RegisteredBitmap)> {
        self.bitmaps.iter()
    }

    /// Returns a placement without allowing map mutation.
    #[cfg(test)]
    pub(crate) fn placement(&self, placement_id: u32) -> Option<&BitmapPlacementState> {
        self.placements.get(&placement_id)
    }

    /// Iterates placements without exposing mutable map access.
    pub(crate) fn placements(&self) -> impl Iterator<Item = (&u32, &BitmapPlacementState)> {
        self.placements.iter()
    }

    /// Reports whether visible bitmap state changed since the last dirty reset.
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns and clears the visible-state dirty flag.
    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Applies terminal upward scrolling to cell-anchored placements.
    pub(crate) fn apply_scroll(&mut self, rows_scrolled: u16) {
        if rows_scrolled == 0 || self.placements.is_empty() {
            return;
        }

        let mut changed = false;
        self.placements.retain(|_, placement| {
            let new_row = placement.row as i64 - rows_scrolled as i64;
            if new_row + placement.rows as i64 <= 0 {
                changed = true;
                return false;
            }
            let row = new_row.max(0) as u16;
            changed |= row != placement.row;
            placement.row = row;
            true
        });
        self.dirty |= changed;
    }

    fn apply_registration(
        &mut self,
        chunk: BitmapRegisterChunk,
    ) -> Result<(), BitmapProtocolError> {
        if self.bitmaps.contains_key(&chunk.bitmap_id) {
            return Err(BitmapProtocolError::new("bitmap ID is already registered"));
        }

        let bitmap_id = chunk.bitmap_id;
        let mut pending = match self.pending_registrations.remove(&bitmap_id) {
            Some(pending) => {
                if chunk
                    .format
                    .as_deref()
                    .is_some_and(|value| value != pending.format)
                    || chunk
                        .source
                        .as_deref()
                        .is_some_and(|value| value != pending.source)
                    || chunk
                        .name
                        .as_ref()
                        .is_some_and(|value| Some(value) != pending.name.as_ref())
                {
                    self.pending_registrations.insert(bitmap_id, pending);
                    return Err(BitmapProtocolError::new(
                        "registration continuation metadata does not match",
                    ));
                }
                pending
            }
            None => PendingBitmapTransfer {
                format: chunk
                    .format
                    .clone()
                    .filter(|value| value == "png")
                    .ok_or_else(|| {
                        BitmapProtocolError::new("first registration chunk requires PNG format")
                    })?,
                source: chunk
                    .source
                    .clone()
                    .filter(|value| value == "payload")
                    .ok_or_else(|| {
                        BitmapProtocolError::new("first registration chunk requires payload source")
                    })?,
                name: chunk.name.clone(),
                data: Vec::new(),
            },
        };

        pending
            .data
            .len()
            .checked_add(chunk.data.len())
            .filter(|length| *length <= MAX_REGISTRATION_BYTES)
            .ok_or_else(|| {
                BitmapProtocolError::new("bitmap registration payload exceeds 64 MiB")
            })?;
        pending.data.extend_from_slice(&chunk.data);

        if chunk.more {
            self.pending_registrations.insert(bitmap_id, pending);
            return Ok(());
        }

        let decoded = image::load_from_memory_with_format(&pending.data, image::ImageFormat::Png)
            .map_err(|_| BitmapProtocolError::new("invalid PNG bitmap payload"))?
            .to_rgba8();
        let (width, height) = decoded.dimensions();
        self.bitmaps.insert(
            bitmap_id,
            RegisteredBitmap {
                width,
                height,
                rgba: decoded.into_raw(),
                handle: None,
            },
        );
        self.dirty = true;
        Ok(())
    }

    fn apply_placement(&mut self, placement: BitmapPlacement) -> Result<(), BitmapProtocolError> {
        if self.placements.contains_key(&placement.placement_id) {
            return Err(BitmapProtocolError::new("placement ID is already in use"));
        }
        let bitmap = self
            .bitmaps
            .get(&placement.bitmap_id)
            .ok_or_else(|| BitmapProtocolError::new("placement bitmap is not registered"))?;
        if placement.columns == 0 || placement.rows == 0 {
            return Err(BitmapProtocolError::new(
                "placement dimensions must be nonzero",
            ));
        }
        let opacity = validate_opacity(placement.opacity)?;
        let source = clamp_source(placement.source, bitmap.width, bitmap.height)?;
        let generation = self
            .next_placement_generation
            .checked_add(1)
            .ok_or_else(|| BitmapProtocolError::new("bitmap placement generation overflow"))?;
        self.next_placement_generation = generation;
        self.placements.insert(
            placement.placement_id,
            BitmapPlacementState {
                generation,
                bitmap_id: placement.bitmap_id,
                row: placement.row,
                col: placement.col,
                columns: placement.columns,
                rows: placement.rows,
                source,
                fit: placement.fit,
                filter: placement.filter,
                opacity,
            },
        );
        self.dirty = true;
        Ok(())
    }

    fn apply_placement_update(
        &mut self,
        placement_id: u32,
        update: BitmapPlacementUpdate,
    ) -> Result<(), BitmapProtocolError> {
        if update == BitmapPlacementUpdate::default() {
            return Err(BitmapProtocolError::new(
                "placement update contains no fields",
            ));
        }
        if update.columns.is_some() != update.rows.is_some() {
            return Err(BitmapProtocolError::new(
                "placement width and height must be updated together",
            ));
        }
        let current = self
            .placements
            .get(&placement_id)
            .ok_or_else(|| BitmapProtocolError::new("placement does not exist"))?;
        let bitmap = self
            .bitmaps
            .get(&current.bitmap_id)
            .ok_or_else(|| BitmapProtocolError::new("placement bitmap is not registered"))?;
        let mut next = current.clone();
        if let Some(row) = update.row {
            next.row = row;
        }
        if let Some(col) = update.col {
            next.col = col;
        }
        if let (Some(columns), Some(rows)) = (update.columns, update.rows) {
            if columns == 0 || rows == 0 {
                return Err(BitmapProtocolError::new(
                    "placement dimensions must be nonzero",
                ));
            }
            next.columns = columns;
            next.rows = rows;
        }
        if let Some(source) = update.source {
            next.source = clamp_source(Some(source), bitmap.width, bitmap.height)?;
        }
        if let Some(fit) = update.fit {
            next.fit = fit;
        }
        if let Some(filter) = update.filter {
            next.filter = filter;
        }
        if let Some(opacity) = update.opacity {
            next.opacity = validate_opacity(opacity)?;
        }
        self.placements.insert(placement_id, next);
        self.dirty = true;
        Ok(())
    }

    fn apply_frame(&mut self, chunk: BitmapFrameChunk) -> Result<(), BitmapProtocolError> {
        let bitmap = self
            .bitmaps
            .get(&chunk.bitmap_id)
            .ok_or_else(|| BitmapProtocolError::new("frame bitmap is not registered"))?;
        let bitmap_dimensions = (bitmap.width, bitmap.height);
        let latest = self.latest_frame_sequences.get(&chunk.bitmap_id).copied();
        if latest.is_some_and(|latest| chunk.sequence <= latest) {
            return Err(BitmapProtocolError::new("stale bitmap frame sequence"));
        }

        let bitmap_id = chunk.bitmap_id;
        let mut pending = match self.pending_frames.remove(&bitmap_id) {
            Some(pending) if chunk.sequence < pending.sequence => {
                self.pending_frames.insert(bitmap_id, pending);
                return Err(BitmapProtocolError::new("stale bitmap frame sequence"));
            }
            Some(pending) if chunk.sequence == pending.sequence => {
                if chunk
                    .format
                    .as_deref()
                    .is_some_and(|value| value != pending.format)
                    || chunk.width.is_some_and(|value| value != pending.width)
                    || chunk.height.is_some_and(|value| value != pending.height)
                {
                    return Err(BitmapProtocolError::new(
                        "frame continuation metadata does not match",
                    ));
                }
                pending
            }
            Some(previous) => match new_pending_frame(&chunk, bitmap_dimensions) {
                Ok(next) => next,
                Err(error) => {
                    self.pending_frames.insert(bitmap_id, previous);
                    return Err(error);
                }
            },
            None => new_pending_frame(&chunk, bitmap_dimensions)?,
        };

        let new_len = match pending.data.len().checked_add(chunk.data.len()) {
            Some(length) if length <= pending.expected_len => length,
            _ => {
                return Err(BitmapProtocolError::new(
                    "bitmap frame payload is too large",
                ));
            }
        };
        pending.data.reserve(new_len - pending.data.len());
        pending.data.extend_from_slice(&chunk.data);
        if chunk.more {
            self.pending_frames.insert(bitmap_id, pending);
            return Ok(());
        }
        if pending.data.len() != pending.expected_len {
            return Err(BitmapProtocolError::new(
                "bitmap frame payload length does not match",
            ));
        }

        self.bitmaps
            .get_mut(&bitmap_id)
            .expect("bitmap existence checked above")
            .rgba = pending.data;
        self.latest_frame_sequences
            .insert(bitmap_id, chunk.sequence);
        self.dirty = true;
        Ok(())
    }

    fn delete_bitmap(&mut self, bitmap_id: u32) {
        if !self.bitmaps.contains_key(&bitmap_id) {
            return;
        }
        self.pending_registrations.remove(&bitmap_id);
        self.pending_frames.remove(&bitmap_id);
        self.latest_frame_sequences.remove(&bitmap_id);
        if self.bitmaps.remove(&bitmap_id).is_some() {
            self.placements
                .retain(|_, placement| placement.bitmap_id != bitmap_id);
            self.dirty = true;
        }
    }

    fn apply_error_cleanup(&mut self, error: &BitmapProtocolError) {
        if let Some(BitmapErrorCleanup::DiscardPendingRegistration { bitmap_id }) =
            error.cleanup.as_ref()
        {
            self.pending_registrations.remove(bitmap_id);
            return;
        }
        let Some(BitmapErrorCleanup::DiscardPendingFrame {
            bitmap_id,
            sequence,
            format,
            width,
            height,
        }) = error.cleanup.as_ref()
        else {
            return;
        };
        let should_discard = self.pending_frames.get(bitmap_id).is_some_and(|pending| {
            if *sequence < pending.sequence {
                return false;
            }
            if *sequence == pending.sequence {
                return true;
            }
            self.bitmaps.get(bitmap_id).is_some_and(|bitmap| {
                format.as_deref() == Some("rgba8")
                    && *width == Some(bitmap.width)
                    && *height == Some(bitmap.height)
            })
        });
        if should_discard {
            self.pending_frames.remove(bitmap_id);
        }
    }
}

fn new_pending_frame(
    chunk: &BitmapFrameChunk,
    bitmap_dimensions: (u32, u32),
) -> Result<PendingBitmapFrame, BitmapProtocolError> {
    let format = chunk
        .format
        .clone()
        .filter(|value| value == "rgba8")
        .ok_or_else(|| BitmapProtocolError::new("first frame chunk requires RGBA8 format"))?;
    let width = chunk
        .width
        .ok_or_else(|| BitmapProtocolError::new("first frame chunk requires width"))?;
    let height = chunk
        .height
        .ok_or_else(|| BitmapProtocolError::new("first frame chunk requires height"))?;
    let expected_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| BitmapProtocolError::new("bitmap frame dimensions overflow"))?;
    if (width, height) != bitmap_dimensions {
        return Err(BitmapProtocolError::new(
            "bitmap frame dimensions must match registered bitmap",
        ));
    }
    Ok(PendingBitmapFrame {
        sequence: chunk.sequence,
        format,
        width,
        height,
        expected_len,
        data: Vec::new(),
    })
}

fn clamp_source(
    source: Option<SourceRect>,
    bitmap_width: u32,
    bitmap_height: u32,
) -> Result<Option<SourceRect>, BitmapProtocolError> {
    let Some(source) = source else {
        return Ok(None);
    };
    if source.width == 0 || source.height == 0 {
        return Err(BitmapProtocolError::new(
            "source dimensions must be nonzero",
        ));
    }
    let end_x = source.x.saturating_add(source.width).min(bitmap_width);
    let end_y = source.y.saturating_add(source.height).min(bitmap_height);
    let start_x = source.x.min(bitmap_width);
    let start_y = source.y.min(bitmap_height);
    if end_x <= start_x || end_y <= start_y {
        return Err(BitmapProtocolError::new(
            "source rectangle is outside bitmap bounds",
        ));
    }
    Ok(Some(SourceRect {
        x: start_x,
        y: start_y,
        width: end_x - start_x,
        height: end_y - start_y,
    }))
}

fn validate_opacity(opacity: f32) -> Result<f32, BitmapProtocolError> {
    if !opacity.is_finite() {
        return Err(BitmapProtocolError::new("bitmap opacity must be finite"));
    }
    Ok(opacity.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUPPORT_REPLY: &[u8] = b"\x1b_ratty;i;s;v=1;fmt=png;frame=rgba8;payload=1;chunk=1;placement=1;crop=1;fit=contain|cover|fill;filter=nearest|linear;opacity=1\x1b\\";
    const PNG_2X2: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72,
        0xb6, 0x0d, 0x24, 0x00, 0x00, 0x00, 0x12, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x0c, 0x81, 0x34, 0x18, 0x00, 0x00, 0x49, 0xc8, 0x09, 0xf7, 0xf9,
        0xab, 0xb6, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    const RGBA_2X2: &[u8] = &[
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];

    fn parse(command: &[u8]) -> Result<BitmapOperation, BitmapProtocolError> {
        consume_sequence(command).expect("bitmap namespace should be consumed")
    }

    fn register_chunk(bitmap_id: u32, data: &[u8], more: bool) -> BitmapOperation {
        BitmapOperation::Register(BitmapRegisterChunk {
            bitmap_id,
            format: Some("png".into()),
            source: Some("payload".into()),
            name: None,
            more,
            data: data.to_vec(),
        })
    }

    fn registration_continuation(bitmap_id: u32, data: &[u8], more: bool) -> BitmapOperation {
        BitmapOperation::Register(BitmapRegisterChunk {
            bitmap_id,
            format: None,
            source: None,
            name: None,
            more,
            data: data.to_vec(),
        })
    }

    fn placement(bitmap_id: u32, placement_id: u32) -> BitmapOperation {
        BitmapOperation::Place(BitmapPlacement {
            bitmap_id,
            placement_id,
            row: 1,
            col: 2,
            columns: 8,
            rows: 4,
            source: None,
            fit: BitmapFit::Contain,
            filter: BitmapFilter::Linear,
            opacity: 1.0,
        })
    }

    fn frame_chunk(bitmap_id: u32, sequence: u32, data: &[u8], more: bool) -> BitmapOperation {
        BitmapOperation::Frame(BitmapFrameChunk {
            bitmap_id,
            sequence,
            format: Some("rgba8".into()),
            width: Some(2),
            height: Some(2),
            more,
            data: data.to_vec(),
        })
    }

    fn frame_continuation(
        bitmap_id: u32,
        sequence: u32,
        data: &[u8],
        more: bool,
    ) -> BitmapOperation {
        BitmapOperation::Frame(BitmapFrameChunk {
            bitmap_id,
            sequence,
            format: None,
            width: None,
            height: None,
            more,
            data: data.to_vec(),
        })
    }

    fn registered_state() -> BitmapSurfaceState {
        let mut state = BitmapSurfaceState::default();
        state
            .apply(register_chunk(1, PNG_2X2, false))
            .expect("valid bitmap test fixture should succeed");
        state.take_dirty();
        state
    }

    #[test]
    fn accepts_both_string_terminators() {
        assert_eq!(
            parse(b"\x1b_ratty;i;s\x1b\\").expect("valid bitmap test fixture should succeed"),
            BitmapOperation::SupportQuery
        );
        assert_eq!(
            parse(b"\x1b_ratty;i;s\x9c").expect("valid bitmap test fixture should succeed"),
            BitmapOperation::SupportQuery
        );
    }

    #[test]
    fn returns_exact_support_reply() {
        assert_eq!(support_reply(), SUPPORT_REPLY);
    }

    #[test]
    fn parses_one_shot_registration() {
        let operation = parse(
            b"\x1b_ratty;i;r;id=42;fmt=png;source=payload;more=0;name=photo.png;aGVsbG8=\x1b\\",
        )
        .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            operation,
            BitmapOperation::Register(BitmapRegisterChunk {
                bitmap_id: 42,
                format: Some("png".into()),
                source: Some("payload".into()),
                name: Some("photo.png".into()),
                more: false,
                data: b"hello".to_vec(),
            })
        );
    }

    #[test]
    fn parses_registration_continuation_chunk() {
        let operation = parse(b"\x1b_ratty;i;r;id=42;more=1;AQID\x9c")
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            operation,
            BitmapOperation::Register(BitmapRegisterChunk {
                bitmap_id: 42,
                format: None,
                source: None,
                name: None,
                more: true,
                data: vec![1, 2, 3],
            })
        );
    }

    #[test]
    fn parses_placement_defaults() {
        let operation = parse(b"\x1b_ratty;i;p;id=42;pid=7;row=4;col=2;w=80;h=30\x1b\\")
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            operation,
            BitmapOperation::Place(BitmapPlacement {
                bitmap_id: 42,
                placement_id: 7,
                row: 4,
                col: 2,
                columns: 80,
                rows: 30,
                source: None,
                fit: BitmapFit::Contain,
                filter: BitmapFilter::Linear,
                opacity: 1.0,
            })
        );
    }

    #[test]
    fn parses_explicit_placement_fields_and_clamps_opacity() {
        let operation = parse(b"\x1b_ratty;i;p;id=42;pid=7;row=4;col=2;w=80;h=30;src_x=3;src_y=5;src_w=20;src_h=10;fit=cover;filter=nearest;opacity=2.5\x1b\\")
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            operation,
            BitmapOperation::Place(BitmapPlacement {
                bitmap_id: 42,
                placement_id: 7,
                row: 4,
                col: 2,
                columns: 80,
                rows: 30,
                source: Some(SourceRect {
                    x: 3,
                    y: 5,
                    width: 20,
                    height: 10,
                }),
                fit: BitmapFit::Cover,
                filter: BitmapFilter::Nearest,
                opacity: 1.0,
            })
        );
    }

    #[test]
    fn parses_full_placement_update() {
        let operation = parse(b"\x1b_ratty;i;u;pid=7;row=8;col=9;w=40;h=20;src_x=3;src_y=5;src_w=20;src_h=10;fit=fill;filter=nearest;opacity=-0.2\x1b\\")
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            operation,
            BitmapOperation::Update {
                placement_id: 7,
                update: BitmapPlacementUpdate {
                    row: Some(8),
                    col: Some(9),
                    columns: Some(40),
                    rows: Some(20),
                    source: Some(SourceRect {
                        x: 3,
                        y: 5,
                        width: 20,
                        height: 10,
                    }),
                    fit: Some(BitmapFit::Fill),
                    filter: Some(BitmapFilter::Nearest),
                    opacity: Some(0.0),
                },
            }
        );
    }

    #[test]
    fn parses_one_shot_frame() {
        let operation =
            parse(b"\x1b_ratty;i;f;id=42;seq=123;fmt=rgba8;w=1;h=1;more=0;AQIDBA==\x1b\\")
                .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            operation,
            BitmapOperation::Frame(BitmapFrameChunk {
                bitmap_id: 42,
                sequence: 123,
                format: Some("rgba8".into()),
                width: Some(1),
                height: Some(1),
                more: false,
                data: vec![1, 2, 3, 4],
            })
        );
    }

    #[test]
    fn parses_frame_continuation_chunk() {
        let operation = parse(b"\x1b_ratty;i;f;id=42;seq=123;more=1;AQID\x9c")
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            operation,
            BitmapOperation::Frame(BitmapFrameChunk {
                bitmap_id: 42,
                sequence: 123,
                format: None,
                width: None,
                height: None,
                more: true,
                data: vec![1, 2, 3],
            })
        );
    }

    #[test]
    fn parses_both_delete_targets() {
        assert_eq!(
            parse(b"\x1b_ratty;i;d;pid=7\x1b\\").expect("valid bitmap test fixture should succeed"),
            BitmapOperation::DeletePlacement(7)
        );
        assert_eq!(
            parse(b"\x1b_ratty;i;d;id=42\x1b\\").expect("valid bitmap test fixture should succeed"),
            BitmapOperation::DeleteBitmap(42)
        );
    }

    #[test]
    fn leaves_other_namespaces_unconsumed() {
        assert_eq!(consume_sequence(b"\x1b_ratty;g;s\x1b\\"), None);
        assert_eq!(consume_sequence(b"plain text"), None);
    }

    #[test]
    fn consumes_unknown_verbs_as_ignored() {
        assert_eq!(
            parse(b"\x1b_ratty;i;x;id=1\x1b\\").expect("valid bitmap test fixture should succeed"),
            BitmapOperation::Ignored
        );
    }

    #[test]
    fn rejects_empty_bitmap_verb() {
        assert!(parse(b"\x1b_ratty;i;\x1b\\").is_err());
    }

    #[test]
    fn rejects_invalid_base64_and_duplicate_keys() {
        assert!(parse(b"\x1b_ratty;i;r;id=1;fmt=png;source=payload;more=0;%%%\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;p;id=1;id=2;pid=3;row=0;col=0;w=1;h=1\x1b\\").is_err());
    }

    #[test]
    fn preflights_registration_payload_size_before_base64_decode() {
        let error = parse_sequence_with_payload_limit(
            b"\x1b_ratty;i;r;id=1;fmt=png;source=payload;more=0;%%%%\x1b\\",
            2,
        )
        .expect_err("three decoded bytes must exceed a two-byte chunk limit");

        assert_eq!(error.to_string(), CHUNK_PAYLOAD_TOO_LARGE);
        assert_eq!(
            error.cleanup,
            Some(BitmapErrorCleanup::DiscardPendingRegistration { bitmap_id: 1 })
        );
    }

    #[test]
    fn rejects_overlong_non_payload_header_before_field_collection() {
        let error = parse_sequence_with_limits(
            b"\x1b_ratty;i;p;id=1;pid=2;row=0;col=0;w=1;h=1\x1b\\",
            MAX_BITMAP_CHUNK_DECODED_BYTES,
            8,
        )
        .expect_err("the complete placement content exceeds the test header limit");

        assert_eq!(error.to_string(), "bitmap APC header exceeds 4 KiB");
    }

    #[test]
    fn rejects_register_and_frame_semicolon_amplification_as_header() {
        for command in [
            b"\x1b_ratty;i;r;;;;;;;;;;;;payload\x1b\\".as_slice(),
            b"\x1b_ratty;i;f;;;;;;;;;;;;payload\x1b\\".as_slice(),
        ] {
            let error = parse_sequence_with_limits(command, MAX_BITMAP_CHUNK_DECODED_BYTES, 8)
                .expect_err("the final payload separator places semicolon runs in the header");

            assert_eq!(error.to_string(), "bitmap APC header exceeds 4 KiB");
        }
    }

    #[test]
    fn accepts_header_at_injected_boundary() {
        assert_eq!(
            parse_sequence_with_limits(b"\x1b_ratty;i;s\x1b\\", MAX_BITMAP_CHUNK_DECODED_BYTES, 1,)
                .expect("one-byte support header equals the test limit"),
            BitmapOperation::SupportQuery
        );
    }

    #[test]
    fn preflights_frame_payload_size_with_sequence_cleanup_metadata() {
        let error = parse_sequence_with_payload_limit(
            b"\x1b_ratty;i;f;id=7;seq=9;fmt=rgba8;w=1;h=1;more=0;%%%%\x1b\\",
            2,
        )
        .expect_err("three decoded bytes must exceed a two-byte chunk limit");

        assert_eq!(error.to_string(), CHUNK_PAYLOAD_TOO_LARGE);
        assert_eq!(
            error.cleanup,
            Some(BitmapErrorCleanup::DiscardPendingFrame {
                bitmap_id: 7,
                sequence: 9,
                format: Some("rgba8".into()),
                width: Some(1),
                height: Some(1),
            })
        );
    }

    #[test]
    fn oversized_frame_chunk_preflight_discards_matching_pending_sequence() {
        let mut state = registered_state();
        state
            .apply(frame_chunk(1, 9, &[1, 2], true))
            .expect("first frame chunk should remain pending");
        let error =
            parse_sequence_with_payload_limit(b"\x1b_ratty;i;f;id=1;seq=9;more=0;%%%%\x1b\\", 2)
                .expect_err("estimated decoded payload exceeds the test chunk limit");

        state.apply_error_cleanup(&error);

        assert!(!state.pending_frames.contains_key(&1));
    }

    #[test]
    fn rejects_missing_ids() {
        assert!(parse(b"\x1b_ratty;i;r;fmt=png;source=payload;more=0;AQ==\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;p;id=1;row=0;col=0;w=1;h=1\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;u;row=1\x1b\\").is_err());
    }

    #[test]
    fn rejects_zero_dimensions_and_non_finite_opacity() {
        assert!(parse(b"\x1b_ratty;i;p;id=1;pid=2;row=0;col=0;w=0;h=1\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;u;pid=2;w=1;h=0\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;p;id=1;pid=2;row=0;col=0;w=1;h=1;opacity=NaN\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;u;pid=2;opacity=inf\x1b\\").is_err());
    }

    #[test]
    fn rejects_partial_source_and_destination_groups() {
        assert!(parse(b"\x1b_ratty;i;p;id=1;pid=2;row=0;col=0;w=1;h=1;src_x=0\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;u;pid=2;src_x=0;src_y=0;src_w=1\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;u;pid=2;w=1\x1b\\").is_err());
    }

    #[test]
    fn rejects_missing_first_frame_metadata() {
        assert!(parse(b"\x1b_ratty;i;f;id=1;seq=2;fmt=rgba8;w=1;more=0;AQIDBA==\x1b\\").is_err());
        assert!(
            parse(b"\x1b_ratty;i;f;id=1;seq=2;fmt=rgba8;w=0;h=1;more=0;AQIDBA==\x1b\\").is_err()
        );
    }

    #[test]
    fn rejects_empty_update_and_ambiguous_delete() {
        assert!(parse(b"\x1b_ratty;i;u;pid=2\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;d\x1b\\").is_err());
        assert!(parse(b"\x1b_ratty;i;d;id=1;pid=2\x1b\\").is_err());
    }

    #[test]
    fn rejects_bad_terminator_inside_bitmap_namespace() {
        assert!(parse(b"\x1b_ratty;i;s").is_err());
        assert!(parse(b"\x1b_ratty;i;s\x1bX").is_err());
    }

    #[test]
    fn decodes_one_shot_png_registration_and_replies_only_to_support() {
        let mut state = BitmapSurfaceState::default();

        assert_eq!(
            state
                .apply(BitmapOperation::SupportQuery)
                .expect("valid bitmap test fixture should succeed"),
            Some(support_reply())
        );
        assert!(!state.is_dirty());
        assert_eq!(
            state
                .apply(register_chunk(1, PNG_2X2, false))
                .expect("valid bitmap test fixture should succeed"),
            None
        );

        let bitmap = state
            .bitmap(1)
            .expect("valid bitmap test fixture should succeed");
        assert_eq!((bitmap.width, bitmap.height), (2, 2));
        assert_eq!(bitmap.rgba, RGBA_2X2);
        assert!(bitmap.handle.is_none());
        assert!(state.is_dirty());
    }

    #[test]
    fn assembles_chunked_png_and_requires_matching_metadata() {
        let mut state = BitmapSurfaceState::default();
        let split = 31;
        state
            .apply(register_chunk(1, &PNG_2X2[..split], true))
            .expect("valid bitmap test fixture should succeed");
        let mismatched = BitmapOperation::Register(BitmapRegisterChunk {
            bitmap_id: 1,
            format: Some("png".into()),
            source: Some("payload".into()),
            name: Some("different.png".into()),
            more: true,
            data: vec![9],
        });
        assert!(state.apply(mismatched).is_err());
        state
            .apply(registration_continuation(1, &PNG_2X2[split..], false))
            .expect("valid bitmap test fixture should succeed");

        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            RGBA_2X2
        );
        assert!(state.pending_registrations.is_empty());
    }

    #[test]
    fn invalid_png_and_registration_overflow_discard_pending_transfer() {
        let mut state = BitmapSurfaceState::default();
        state
            .apply(register_chunk(1, b"not ", true))
            .expect("valid bitmap test fixture should succeed");
        assert!(
            state
                .apply(registration_continuation(1, b"png", false))
                .is_err()
        );
        assert!(!state.pending_registrations.contains_key(&1));
        assert!(state.bitmap(1).is_none());

        let oversized = vec![0; MAX_REGISTRATION_BYTES + 1];
        assert!(state.apply(register_chunk(2, &oversized, true)).is_err());
        assert!(!state.pending_registrations.contains_key(&2));
        assert!(state.bitmap(2).is_none());
    }

    #[test]
    fn duplicate_bitmap_id_does_not_mutate_existing_state() {
        let mut state = registered_state();
        let before = state
            .bitmap(1)
            .expect("valid bitmap test fixture should succeed")
            .rgba
            .clone();

        assert!(
            state
                .apply(register_chunk(1, b"replacement", false))
                .is_err()
        );

        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            before
        );
        assert!(!state.is_dirty());
    }

    #[test]
    fn placement_requires_bitmap_supports_siblings_and_rejects_duplicate_id() {
        let mut state = registered_state();
        assert!(state.apply(placement(99, 10)).is_err());
        state
            .apply(placement(1, 10))
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(placement(1, 11))
            .expect("valid bitmap test fixture should succeed");
        let before = state
            .placement(10)
            .expect("valid bitmap test fixture should succeed")
            .clone();

        assert!(state.apply(placement(1, 10)).is_err());

        assert_eq!(state.placements().count(), 2);
        assert_eq!(state.placement(10), Some(&before));
    }

    #[test]
    fn placement_generations_advance_only_for_successful_new_lifetimes() {
        let mut state = registered_state();
        state
            .apply(placement(1, 10))
            .expect("valid bitmap test fixture should succeed");
        let first_generation = state
            .placement(10)
            .expect("valid bitmap test fixture should succeed")
            .generation();

        state
            .apply(BitmapOperation::Update {
                placement_id: 10,
                update: BitmapPlacementUpdate {
                    opacity: Some(0.5),
                    ..BitmapPlacementUpdate::default()
                },
            })
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            state
                .placement(10)
                .expect("valid bitmap test fixture should succeed")
                .generation(),
            first_generation
        );
        assert!(state.apply(placement(1, 10)).is_err());

        state
            .apply(BitmapOperation::DeletePlacement(10))
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(placement(1, 10))
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            state
                .placement(10)
                .expect("valid bitmap test fixture should succeed")
                .generation(),
            first_generation + 1
        );
    }

    #[test]
    fn placement_clamps_source_rect_and_rejects_empty_intersection() {
        let mut state = registered_state();
        let mut clamped = match placement(1, 10) {
            BitmapOperation::Place(value) => value,
            _ => unreachable!(),
        };
        clamped.source = Some(SourceRect {
            x: 1,
            y: 1,
            width: u32::MAX,
            height: 20,
        });
        state
            .apply(BitmapOperation::Place(clamped))
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            state
                .placement(10)
                .expect("valid bitmap test fixture should succeed")
                .source,
            Some(SourceRect {
                x: 1,
                y: 1,
                width: 1,
                height: 1
            })
        );

        let mut empty = match placement(1, 11) {
            BitmapOperation::Place(value) => value,
            _ => unreachable!(),
        };
        empty.source = Some(SourceRect {
            x: 2,
            y: 0,
            width: 1,
            height: 1,
        });
        assert!(state.apply(BitmapOperation::Place(empty)).is_err());
        assert!(state.placement(11).is_none());
    }

    #[test]
    fn placement_update_is_transactional_and_clamps_opacity() {
        let mut state = registered_state();
        state
            .apply(placement(1, 10))
            .expect("valid bitmap test fixture should succeed");
        state.take_dirty();
        let before = state
            .placement(10)
            .expect("valid bitmap test fixture should succeed")
            .clone();

        let invalid = BitmapPlacementUpdate {
            row: Some(9),
            columns: Some(0),
            rows: Some(5),
            ..Default::default()
        };
        assert!(
            state
                .apply(BitmapOperation::Update {
                    placement_id: 10,
                    update: invalid
                })
                .is_err()
        );
        assert_eq!(state.placement(10), Some(&before));
        assert!(!state.is_dirty());

        let valid = BitmapPlacementUpdate {
            row: Some(9),
            opacity: Some(4.0),
            ..Default::default()
        };
        state
            .apply(BitmapOperation::Update {
                placement_id: 10,
                update: valid,
            })
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            state
                .placement(10)
                .expect("valid bitmap test fixture should succeed")
                .row,
            9
        );
        assert_eq!(
            state
                .placement(10)
                .expect("valid bitmap test fixture should succeed")
                .opacity,
            1.0
        );
        assert!(
            state
                .apply(BitmapOperation::Update {
                    placement_id: 404,
                    update: Default::default()
                })
                .is_err()
        );
    }

    #[test]
    fn applies_one_shot_and_chunked_rgba8_frames() {
        let mut state = registered_state();
        let first = [7; 16];
        state
            .apply(frame_chunk(1, 1, &first, false))
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            first
        );

        let second = [8; 16];
        state
            .apply(frame_chunk(1, 2, &second[..7], true))
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(frame_continuation(1, 2, &second[7..], false))
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            second
        );
        assert!(state.pending_frames.is_empty());
    }

    #[test]
    fn frame_validates_exact_length_dimensions_and_checked_expected_size() {
        let mut state = registered_state();
        let original = state
            .bitmap(1)
            .expect("valid bitmap test fixture should succeed")
            .rgba
            .clone();
        assert!(state.apply(frame_chunk(1, 1, &[1; 15], false)).is_err());
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            original
        );

        let mut wrong_dimensions = match frame_chunk(1, 2, &[2; 16], false) {
            BitmapOperation::Frame(value) => value,
            _ => unreachable!(),
        };
        wrong_dimensions.width = Some(3);
        assert!(
            state
                .apply(BitmapOperation::Frame(wrong_dimensions))
                .is_err()
        );

        let overflow = BitmapFrameChunk {
            bitmap_id: 1,
            sequence: 3,
            format: Some("rgba8".into()),
            width: Some(u32::MAX),
            height: Some(u32::MAX),
            more: true,
            data: vec![],
        };
        assert!(state.apply(BitmapOperation::Frame(overflow)).is_err());
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            original
        );
    }

    #[test]
    fn newer_frame_cancels_incomplete_older_and_rejects_stale_sequences() {
        let mut state = registered_state();
        state
            .apply(frame_chunk(1, 10, &[1; 4], true))
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(frame_chunk(1, 11, &[2; 16], false))
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            [2; 16]
        );
        assert!(
            state
                .apply(frame_continuation(1, 10, &[1; 12], false))
                .is_err()
        );
        assert!(state.apply(frame_chunk(1, 11, &[3; 16], false)).is_err());
        assert!(state.apply(frame_chunk(1, 9, &[4; 16], false)).is_err());
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            [2; 16]
        );
    }

    #[test]
    fn malformed_newer_frame_does_not_cancel_an_incomplete_valid_frame() {
        let mut state = registered_state();
        state
            .apply(frame_chunk(1, 10, &[1; 4], true))
            .expect("valid bitmap test fixture should succeed");

        assert!(
            state
                .apply(frame_continuation(1, 11, &[2; 4], true))
                .is_err()
        );
        state
            .apply(frame_continuation(1, 10, &[1; 12], false))
            .expect("valid bitmap test fixture should succeed");

        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            [1; 16]
        );
    }

    #[test]
    fn invalid_base64_continuation_discards_matching_pending_frame() {
        let mut state = registered_state();
        let original = state
            .bitmap(1)
            .expect("valid bitmap test fixture should succeed")
            .rgba
            .clone();
        state
            .apply(frame_chunk(1, 10, &[1; 4], true))
            .expect("valid bitmap test fixture should succeed");

        let result = state
            .consume_and_apply(b"\x1b_ratty;i;f;id=1;seq=10;more=0;%%%\x1b\\")
            .expect("valid bitmap test fixture should succeed");

        assert!(result.is_err());
        assert!(!state.pending_frames.contains_key(&1));
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            original
        );
    }

    #[test]
    fn invalid_base64_newer_first_chunk_cancels_older_pending_frame() {
        let mut state = registered_state();
        let original = state
            .bitmap(1)
            .expect("valid bitmap test fixture should succeed")
            .rgba
            .clone();
        state
            .apply(frame_chunk(1, 10, &[1; 4], true))
            .expect("valid bitmap test fixture should succeed");

        let result = state
            .consume_and_apply(b"\x1b_ratty;i;f;id=1;seq=11;fmt=rgba8;w=2;h=2;more=0;%%%\x1b\\")
            .expect("valid bitmap test fixture should succeed");

        assert!(result.is_err());
        assert!(!state.pending_frames.contains_key(&1));
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            original
        );
    }

    #[test]
    fn invalid_base64_newer_chunk_without_metadata_preserves_older_pending_frame() {
        let mut state = registered_state();
        state
            .apply(frame_chunk(1, 10, &[1; 4], true))
            .expect("valid bitmap test fixture should succeed");

        let result = state
            .consume_and_apply(b"\x1b_ratty;i;f;id=1;seq=11;more=0;%%%\x1b\\")
            .expect("valid bitmap test fixture should succeed");

        assert!(result.is_err());
        assert_eq!(
            state
                .pending_frames
                .get(&1)
                .expect("valid bitmap test fixture should succeed")
                .sequence,
            10
        );
    }

    #[test]
    fn invalid_base64_newer_chunk_with_wrong_dimensions_preserves_older_pending_frame() {
        let mut state = registered_state();
        state
            .apply(frame_chunk(1, 10, &[1; 4], true))
            .expect("valid bitmap test fixture should succeed");

        let result = state
            .consume_and_apply(b"\x1b_ratty;i;f;id=1;seq=11;fmt=rgba8;w=3;h=2;more=0;%%%\x1b\\")
            .expect("valid bitmap test fixture should succeed");

        assert!(result.is_err());
        assert_eq!(
            state
                .pending_frames
                .get(&1)
                .expect("valid bitmap test fixture should succeed")
                .sequence,
            10
        );
    }

    #[test]
    fn invalid_base64_stale_frame_does_not_cancel_newer_pending_frame() {
        let mut state = registered_state();
        state
            .apply(frame_chunk(1, 11, &[1; 4], true))
            .expect("valid bitmap test fixture should succeed");

        let result = state
            .consume_and_apply(b"\x1b_ratty;i;f;id=1;seq=10;more=0;%%%\x1b\\")
            .expect("valid bitmap test fixture should succeed");

        assert!(result.is_err());
        assert_eq!(
            state
                .pending_frames
                .get(&1)
                .expect("valid bitmap test fixture should succeed")
                .sequence,
            11
        );
    }

    #[test]
    fn corrupt_frame_discards_transfer_and_preserves_last_valid_pixels() {
        let mut state = registered_state();
        let original = state
            .bitmap(1)
            .expect("valid bitmap test fixture should succeed")
            .rgba
            .clone();
        state
            .apply(frame_chunk(1, 1, &[1; 12], true))
            .expect("valid bitmap test fixture should succeed");
        assert!(
            state
                .apply(frame_continuation(1, 1, &[2; 8], true))
                .is_err()
        );

        assert!(!state.pending_frames.contains_key(&1));
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            original
        );
        state
            .apply(frame_chunk(1, 2, &[3; 16], false))
            .expect("valid bitmap test fixture should succeed");
        assert_eq!(
            state
                .bitmap(1)
                .expect("valid bitmap test fixture should succeed")
                .rgba,
            [3; 16]
        );
    }

    #[test]
    fn deletes_are_idempotent_and_bitmap_delete_cascades() {
        let mut state = registered_state();
        state
            .apply(placement(1, 10))
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(placement(1, 11))
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(BitmapOperation::DeletePlacement(10))
            .expect("valid bitmap test fixture should succeed");
        assert!(state.placement(10).is_none());
        assert!(state.placement(11).is_some());
        assert!(state.bitmap(1).is_some());

        state
            .apply(BitmapOperation::DeletePlacement(404))
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(BitmapOperation::DeleteBitmap(404))
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(BitmapOperation::Ignored)
            .expect("valid bitmap test fixture should succeed");
        state
            .apply(BitmapOperation::DeleteBitmap(1))
            .expect("valid bitmap test fixture should succeed");
        assert!(state.bitmap(1).is_none());
        assert!(state.placement(11).is_none());
        assert_eq!(state.bitmaps().count(), 0);
        assert_eq!(state.placements().count(), 0);
    }

    #[test]
    fn deleting_unknown_bitmap_preserves_its_pending_registration() {
        let mut state = BitmapSurfaceState::default();
        state
            .apply(register_chunk(42, &PNG_2X2[..20], true))
            .expect("first registration chunk should remain pending");

        state
            .apply(BitmapOperation::DeleteBitmap(42))
            .expect("deleting an unknown bitmap is a no-op");

        assert!(state.pending_registrations.contains_key(&42));
        state
            .apply(registration_continuation(42, &PNG_2X2[20..], false))
            .expect("pending registration should still be completable");
        assert!(state.bitmap(42).is_some());
    }
}
