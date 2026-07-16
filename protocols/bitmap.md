# Ratty Bitmap Surface Protocol

Ratty Bitmap Surface is a terminal protocol for registering 2D bitmap assets,
placing them in terminal cell space, changing placement and crop properties
without re-uploading pixels, and replacing live pixels without changing the
bitmap identity.

Version 1 uses the `ratty;i` APC namespace. It supports PNG registration and
full-frame RGBA8 replacement only.

## Transport and framing

Commands use APC (Application Program Command) framing:

```text
ESC _ ratty;i;<verb>[;<key=value>...][;<base64-payload>] ESC \
```

Both the two-byte `ESC \` string terminator and the single-byte C1 ST
terminator are accepted. Header fields are semicolon-separated. A command with
a payload places its base64 data after the header fields as the final
semicolon-separated item.

Each individual `r` or `f` APC chunk may decode to at most 64 MiB. Ratty
preflights the encoded payload length before base64 decoding. The v1 encoded
APC bound is derived as
`len(ESC _ ratty;i;) + 4096 + 4 * ceil(64 MiB / 3) + len(ESC \)`: 4096 bytes
are reserved for the verb, fields, and separators, and the two-byte terminator
is the larger accepted terminator. The verb, fields, and separators may occupy
at most those 4096 bytes. For `r` and `f`, the header extends through the final
semicolon that separates the payload, so semicolon runs in an alleged payload
cannot bypass the header limit. A client must split a transfer before any
individual command reaches the complete APC bound.

If an unterminated bitmap APC reaches the encoded bound, Ratty discards bytes
through the next `ESC \` or C1 ST without retaining or displaying them, then
resumes normal terminal parsing after the terminator. This bound applies only
to the `ratty;i` namespace; it does not change RGP or Kitty limits.

Bitmap IDs, placement IDs, sequence numbers, source coordinates, and dimensions
are unsigned decimal `u32` values on the wire. Destination `row` and `col` are
unsigned decimal values limited to the `u16` range `0..=65535`. A bitmap ID is
written as `id`; a placement ID is written as `pid`. Bitmap and placement IDs
each belong to a single global namespace of their kind. Destination `w` and
`h`, source `src_w` and `src_h`, and frame `w` and `h` must be nonzero; IDs,
coordinates, and sequence numbers have no additional nonzero constraint.
Opacity is a finite decimal floating-point value whose effective value is in
`[0,1]`; finite input outside that range is clamped to the nearest endpoint.

The verbs are:

- `s`: query support
- `r`: register a bitmap
- `p`: create a placement
- `u`: update a placement
- `f`: replace a bitmap frame
- `d`: delete a placement or bitmap

## Support discovery

A client queries support with:

```text
ESC _ ratty;i;s ESC \
```

Ratty replies with exactly:

```text
ESC _ ratty;i;s;v=1;fmt=png;frame=rgba8;payload=1;chunk=1;placement=1;crop=1;fit=contain|cover|fill;filter=nearest|linear;opacity=1 ESC \
```

The reply advertises protocol version 1, PNG payload registration, RGBA8 frame
replacement, chunked transfers, independently addressable placements, source
cropping, the three fit modes, the two filter modes, and opacity. If no reply
arrives, the client must assume that Ratty Bitmap Surface is unsupported.
Support queries are the only version 1 commands that produce a reply.

## Coordinate systems and placement model

A registered bitmap owns one pixel image and one stable bitmap identity. It may
have multiple placements, and each placement has a globally unique `pid`.
Registration and frame replacement operate on the shared bitmap; placement and
update commands operate on one placement.

Destination `row`, `col`, `w`, and `h` are measured in terminal cells.
`row,col` is the top-left placement anchor, `w` is the number of columns, and
`h` is the number of rows. Source `src_x`, `src_y`, `src_w`, and `src_h` are
measured in source pixels from the bitmap's top-left origin.

When no source rectangle is specified, the full bitmap is used. A supplied
source rectangle is clamped to the bitmap bounds. The command is rejected if
the clamped intersection is empty. Source and destination widths and heights
must be nonzero.

## Register bitmap (`r`)

Registration carries a base64-encoded PNG payload. A one-chunk registration is:

```text
ESC _ ratty;i;r;id=42;fmt=png;source=payload;more=0;<base64-png> ESC \
```

The first chunk requires:

- `id`: the bitmap ID
- `fmt=png`: the version 1 registration format
- `source=payload`: the version 1 registration source
- `more`: `1` when more chunks follow or `0` on the final chunk
- a base64 payload item

`name` is optional diagnostic metadata. For a multi-chunk registration, later
chunks use the same `id`, include `more`, and carry the next base64 payload
item. `fmt`, `source`, and `name` may be repeated after the first chunk only
when their values exactly match the first chunk.

```text
ESC _ ratty;i;r;id=42;fmt=png;source=payload;more=1;name=photo.png;<chunk-1> ESC \
ESC _ ratty;i;r;id=42;more=1;<chunk-2> ESC \
ESC _ ratty;i;r;id=42;more=0;<chunk-n> ESC \
```

After the per-chunk size preflight, Ratty base64-decodes each chunk and retains
decoded bytes while `more=1`.
`more=0` finalizes the transfer: Ratty decodes the accumulated PNG exactly
once, creates the bitmap, and clears the pending transfer. The bitmap becomes
visible to placement commands only after successful finalization.

A pending registration may contain at most 64 MiB of decoded payload bytes. If
a chunk would exceed that limit, Ratty rejects the chunk and discards the whole
pending registration. An invalid PNG on finalization also clears the pending
transfer and does not register a bitmap. Other malformed chunks make no state
change.

Registering an `id` that is already registered is rejected and never replaces
the existing bitmap or its placements.

## Place bitmap (`p`)

A placement refers to an already registered bitmap and receives its own
globally unique placement ID:

```text
ESC _ ratty;i;p;id=42;pid=7;row=4;col=2;w=80;h=30;fit=contain;filter=linear;opacity=1 ESC \
```

Required fields are `id`, `pid`, `row`, `col`, `w`, and `h`. The destination
dimensions must be nonzero. The optional placement fields are:

- the complete `src_x`, `src_y`, `src_w`, `src_h` source rectangle
- `fit=contain|cover|fill`, default `contain`
- `filter=nearest|linear`, default `linear`
- `opacity`, default `1`

A source rectangle, when present, must contain all four source fields. Opacity
must be finite and is clamped to the inclusive range `[0,1]`.

Placement fails without mutation when the bitmap does not exist or `pid` is
already in use. A bitmap can have any number of distinct placements.

## Update placement (`u`)

An update changes an existing placement without re-registering the bitmap or
changing its bitmap ID:

```text
ESC _ ratty;i;u;pid=7;src_x=300;src_y=120;src_w=900;src_h=600 ESC \
```

`pid` and at least one mutable field are required. Mutable fields are:

- `row` or `col`, independently
- `w` and `h`, as a complete pair
- `src_x`, `src_y`, `src_w`, and `src_h`, as a complete quartet
- `fit`
- `filter`
- `opacity`

Updated destination dimensions must be nonzero. Updated source rectangles use
the same clamping and nonempty-intersection rules as placement. Updated opacity
must be finite and is clamped to `[0,1]`.

Updates are transactional. A partial `w/h` pair, partial source quartet,
unknown placement, invalid value, or failed validation rejects the whole
command and leaves the placement unchanged.

## Replace frame (`f`)

Frame replacement changes the pixels of a registered bitmap while retaining
its identity and every placement:

```text
ESC _ ratty;i;f;id=42;seq=123;fmt=rgba8;w=1280;h=720;more=0;<base64-rgba> ESC \
```

The first chunk requires `id`, `seq`, `fmt=rgba8`, `w`, `h`, `more`, and a
base64 payload item. The dimensions must exactly match the registered bitmap's
dimensions. Continuation chunks require `id`, `seq`, `more`, and the next
payload item. If `fmt`, `w`, or `h` is repeated on a continuation, it must
exactly match the first chunk.

Chunks are assembled by `(id, seq)`. `seq` is mandatory and must increase for
each bitmap. When a newer sequence begins, Ratty discards any incomplete older
sequence for that bitmap. A stale sequence never replaces displayed pixels.

Ratty calculates the expected byte length as `w * h * 4` using checked
arithmetic. Arithmetic overflow rejects the frame. If accumulated decoded data
ever exceeds that expected length, Ratty immediately rejects the chunk and
discards the affected pending `(id, seq)` frame instead of retaining excess
data.

On the final `more=0` chunk, the decoded payload length must equal the expected
byte length. Pixels are tightly packed RGBA8 in row-major order from the
top-left. After all validation succeeds, Ratty atomically replaces the pixels
of the existing bitmap. Its bitmap ID, underlying image handle, dimensions,
and all placements remain unchanged.

Invalid base64, inconsistent continuation metadata, an oversized accumulated
payload, or a final length mismatch discards the affected pending `(id, seq)`
frame. Missing metadata, zero or mismatched dimensions, arithmetic overflow,
stale sequencing, or any other malformed frame is rejected. Every frame error
preserves the last valid displayed pixels and all placements.

## Delete (`d`)

Delete one placement with:

```text
ESC _ ratty;i;d;pid=7 ESC \
```

Delete one bitmap with:

```text
ESC _ ratty;i;d;id=42 ESC \
```

Exactly one of `pid` or `id` is required. A command with neither ID or both IDs
is malformed and does not delete anything. Deleting an unknown placement or
bitmap is an idempotent no-op. In particular, deleting an unknown bitmap ID
does not cancel a pending registration for that ID. Bitmap deletion cascades
only when the bitmap is already registered.

Deleting a placement does not affect its bitmap or sibling placements.
Deleting a bitmap atomically deletes all placements that refer to it.

## Fit and filtering rules

Fit is resolved from the selected source rectangle into the destination:

- `fill`: map the full source rectangle to the full destination; aspect ratio
  may change.
- `contain`: preserve aspect ratio, center the image, and leave transparent
  letterboxing in the unused destination area.
- `cover`: preserve aspect ratio and fill the destination by applying a
  symmetric crop to the source.

`nearest` selects nearest-neighbor sampling. `linear` selects linear sampling.
Filtering belongs to the placement, so placements that share a bitmap may use
different filter modes.

## Errors and mutation rules

Ratty consumes commands in the `ratty;i` namespace even when they are
malformed or unsupported. It logs a warning and sends no error reply. A
malformed or unsupported command makes no state change, except that a failed or
overflowed pending transfer is discarded as described above.

Unknown verbs do nothing. Duplicate keys, unsupported values, missing required
fields, invalid base64, and invalid numeric values are malformed. Only a valid
support query generates output.

Registration, placement, and frame state are separate. Placement changes never
create or replace bitmap pixels. Frame changes never alter placement records.
No version 1 operation accepts filesystem paths, compressed image formats other
than registration PNG, dirty rectangles, codecs, or network sources.

## Complete example

Query support:

```text
ESC _ ratty;i;s ESC \
ESC _ ratty;i;s;v=1;fmt=png;frame=rgba8;payload=1;chunk=1;placement=1;crop=1;fit=contain|cover|fill;filter=nearest|linear;opacity=1 ESC \
```

Register a PNG bitmap, potentially using repeated chunks with the same ID:

```text
ESC _ ratty;i;r;id=42;fmt=png;source=payload;more=0;<base64-png> ESC \
```

Place it in terminal cell space:

```text
ESC _ ratty;i;p;id=42;pid=7;row=4;col=2;w=80;h=30;fit=contain;filter=linear;opacity=1 ESC \
```

Change the placement's source crop without uploading the PNG again:

```text
ESC _ ratty;i;u;pid=7;src_x=300;src_y=120;src_w=900;src_h=600 ESC \
```

Replace the bitmap's pixels with a sequenced, fixed-dimension RGBA8 frame:

```text
ESC _ ratty;i;f;id=42;seq=123;fmt=rgba8;w=1280;h=720;more=0;<base64-rgba> ESC \
```

Delete the placement and then the bitmap:

```text
ESC _ ratty;i;d;pid=7 ESC \
ESC _ ratty;i;d;id=42 ESC \
```
