# Ratty Graphics Protocol

Ratty Graphics Protocol (RGP) is a custom terminal protocol for inserting
3D objects into the terminal as first-class inline objects.

The goal is to attach a semantic graphics object to terminal cells,
so it becomes part of the terminal surface rather than an external overlay.

It is inspired by [TempleOS]-style inline document graphics ([DolDoc])
and by modern terminal extension work such as [Glyph Protocol],
but it is designed for Ratty's inline object layer and 3D renderer.

## Design Goals

- Support 3D assets directly.
- Anchor objects to terminal cell positions.
- Make graphics native terminal objects, not external overlays.
- Allow future interactive behavior such as clicking, macros and object state updates.

## Transport

Ratty Graphics Protocol uses [APC] (Application Program Command):

```text
ESC _ ratty;g;<verb>[;<key=value>...] ESC \
```

Where:

- `ratty` is the protocol namespace
- `g` means graphics
- [`<verb>`](#verbs) selects the operation
- additional fields are semicolon-separated `key=value` pairs

## Model

Ratty treats protocol objects as inline terminal objects.

Each object has:

- an object id
- an anchor cell
- a cell span
- a renderable payload
- optional metadata for future interaction

## Verbs

- `s` [support query](#1-support-query)
- `r` [register object asset](#2-register-object-asset)
- `p` [place object](#3-place-object)
- `u` [update object](#4-update-object)
- `d` [delete object](#5-delete-object)
- `c` [camera control](#6-camera-control)

### 1. Support Query

Used to detect protocol support and version.

Client sends:

```text
ESC _ ratty;g;s ESC \
```

Ratty replies:

```text
ESC _ ratty;g;s;v=1;fmt=obj|glb|stl;path=1;payload=1;chunk=1;anim=1;depth=1;color=1;brightness=1;transform=1;update=1;normalize=1;camera=1 ESC \
```

Fields:

- `v=1`: protocol version
- `fmt=glb`: `obj`, `glb` and `stl` are supported
- `path=1`: path-based object registration is supported
- `payload=1`: payload-based asset registration is supported
- `chunk=1`: chunked payload-based registration is supported
- `anim=1`: `animate=1` placement is supported
- `depth=1`: `depth=<f32>` placement is supported
- `color=1`: `color=<RRGGBB>` placement is supported
- `brightness=1`: `brightness=<f32>` placement is supported
- `camera=1`: `c` camera updates are supported
- `transform=1`: transform fields such as rotation and offsets are supported
- `update=1`: `u` object updates are supported
- `normalize=1`: `normalize=<0|1>` registration is supported for OBJ assets

If no reply arrives, the terminal does not support the protocol.

### 2. Register Object Asset

Registers a 3D object by id.

Client sends:

```text
ESC _ ratty;g;r;id=42;fmt=obj;path=CairoSpinyMouse.obj ESC \
```

This registers object `42` using an object asset.

The required fields are:

- `id`: object id chosen by the application
- `fmt`: payload format, `obj`, `glb`, or `stl` in v1
- `path`: object path known to Ratty

Path-based registration accepts embedded objects by name and files beneath an `assets`
directory in the current working tree or Ratty's runtime asset directory. Parent-directory
traversal is rejected. glTF files must embed their buffers and images.

Optional registration fields:

- `normalize`: OBJ normalization flag, defaults to `1`
  - `1`: center each OBJ mesh around its bounding-box center and scale it by
    the largest bounding-box axis
  - `0`: preserve the OBJ's authored vertex coordinates

#### Payload-based registration

RGP can also register an object by embedding the asset data directly into the
register command as a payload. This is intended for cases such as SSH, where
the sending application cannot rely on a shared filesystem path on the terminal
side.

The payload is base64-encoded and appended after the semicolon-separated
header fields.

Client sends:

```text
ESC _ ratty;g;r;id=42;fmt=obj;source=payload;more=0;name=rat.obj;<base64 payload> ESC \
```

For larger assets, the payload can be split across multiple register chunks:

```text
ESC _ ratty;g;r;id=42;fmt=glb;source=payload;more=1;<chunk-1> ESC \
ESC _ ratty;g;r;id=42;fmt=glb;source=payload;more=1;<chunk-2> ESC \
ESC _ ratty;g;r;id=42;fmt=glb;source=payload;more=0;<chunk-n> ESC \
```

Fields:

- `id`: object id chosen by the application
- `fmt`: payload format, `obj`, `glb`, or `stl`
- `source`: registration source
  - `payload`: asset bytes are carried in this command
- `more`: continuation flag
  - `1`: more register chunks follow for this object id
  - `0`: this is the final chunk and registration can be finalized
- `name`: optional source name for diagnostics and temporary asset naming
- `normalize`: optional OBJ normalization flag on the first payload chunk, defaults to `1`

The terminal accumulates chunks for the same `id` until it receives the final
`more=0` chunk. At that point, the object becomes registered and can be placed
normally.

Path-based and payload-based registration are additive modes of the same `r` verb.
Clients may continue using `path=...` exactly as before.

### 3. Place Object

Places a previously registered object into terminal cell space.

Client sends:

```text
ESC _ ratty;g;p;id=42;row=12;col=8;w=4;h=2;animate=1;scale=1.0;depth=2.5;color=ff8844;brightness=1.0;px=0;py=0;pz=0;rx=0;ry=45;rz=0;sx=1;sy=1;sz=1 ESC \
```

Fields:

- `id`: registered object id
- `row`: anchor row at the center of the placement
- `col`: anchor column at the center of the placement
- `w`: width in terminal cells
- `h`: height in terminal cells
- `animate`: optional, `1` enables default animation
- `scale`: optional scale factor, defaults to `1.0`
- `depth`: optional z-offset, defaults to `0.0`
- `color`: optional RGB color as `RRGGBB`
- `brightness`: optional brightness multiplier, defaults to `1.0`
- `px`, `py`, `pz`: optional translation offset relative to the anchor, defaults to `0`
- `rx`, `ry`, `rz`: optional rotation in degrees, defaults to `0`
- `sx`, `sy`, `sz`: optional non-uniform scale, defaults to `1`

Clients that only send the original v1 fields still work unchanged.

### 4. Update Object

Updates the styling or transform of a previously placed object without changing
its registration or anchor.

Client sends:

```text
ESC _ ratty;g;u;id=42;ry=120;px=0.25;animate=0 ESC \
```

Fields are optional and mirror the mutable fields from `p`:

- `animate`
- `scale`
- `depth`
- `color`
- `brightness`
- `px`, `py`, `pz`
- `rx`, `ry`, `rz`
- `sx`, `sy`, `sz`

### 5. Delete Object

Deletes either a placement or an object.

Delete one object:

```text
ESC _ ratty;g;d;id=42 ESC \
```

Delete all Ratty graphics objects:

```text
ESC _ ratty;g;d ESC \
```

### 6. Camera control

Partially updates one of ten persistent camera presets. `id` is required and
must be a decimal slot number from `0` through `9`.

```text
ESC _ ratty;g;c;id=0;set=0;type=Ortho;px=0.25;scale=1.0 ESC \
ESC _ ratty;g;c;id=1;set=1;type=Persp;fov=50;rx=10;ry=20 ESC \
```

- `set`: `0` or `1`; when `1`, activates the slot after applying this update.
  Defaults to `0`.

Optional fields retain their previous values when omitted:

- `type`: one of `Flat`, `Ortho`, `Persp`, `Mobius`.
- `scale`: orthographic projection scale of at least `0.01`. The value is used
  when the slot presents in `Ortho` or `Mobius`.
- `fov`: vertical perspective field of view in degrees, used when the slot
  presents in `Persp`. The value must be strictly inside the range equivalent
  to `0.05` through `pi - 0.05` radians (about `2.87` through `177.13`
  degrees).
- `px`, `py`, `pz`: horizontal pan, vertical pan, and camera dolly relative to
  the default camera position.
- `rx`, `ry`, `rz`: pitch (X), yaw (Y), and roll (Z), in degrees.

Every angle on the wire (`rx`, `ry`, `rz`, and `fov`) is expressed in degrees.

Orthographic scale and perspective FOV are stored independently per slot, so
one command may set both `scale` and `fov` regardless of the current or
requested `type`, and changing `type` never reinterprets the other mode's
projection value.

Malformed or non-finite numeric values, fields without a value, invalid `id`,
invalid `set`, and unknown `type` values cause the camera command to be
ignored. A valid partial command still applies its other fields when `scale`
or `fov` is numeric but outside its valid range; such an out-of-range value
also does not block `set=1` activation — the slot activates with its previous
projection value.

## Example Session

Register an embedded object path:

```text
ESC _ ratty;g;r;id=7;fmt=obj;path=CairoSpinyMouse.obj ESC \
```

Place it in the terminal at row 5, column 10, spanning 3×2 cells:

```text
ESC _ ratty;g;p;id=7;row=5;col=10;w=3;h=2;animate=1;scale=1.0;depth=1.5;color=7fd0ff;brightness=1.0;ry=30 ESC \
```

Rotate it later:

```text
ESC _ ratty;g;u;id=7;ry=180 ESC \
```

View the terminal in perspective with a 50-degree field of view:

```text
ESC _ ratty;g;c;id=0;set=1;type=Persp;fov=50;rx=10;ry=20 ESC \
```

Return to the flat terminal:

```text
ESC _ ratty;g;c;id=0;set=1;type=Flat ESC \
```

Delete it:

```text
ESC _ ratty;g;d;id=7 ESC \
```

## Summary

Ratty Graphics Protocol is a terminal-native object protocol for 3D graphics.

Its key ideas are:

- register a renderable object
- place it in terminal cell space
- let Ratty render it as part of the terminal, including 3D mode

That is the foundation for "sprites on the command line" in Ratty, inspired by [TempleOS]-style
inline graphics but designed for modern terminal capabilities and 3D rendering.

[TempleOS]: https://templeos.org
[DolDoc]: https://tinkeros.github.io/WbTempleOS/Doc/DolDocOverview.html
[Glyph Protocol]: https://rapha.land/introducing-glyph-protocol-for-terminals/
[APC]: https://en.wikipedia.org/wiki/C0_and_C1_control_codes#C1_controls

## Buffering and synchronized output

Each encoded APC command is limited to 1 MiB. Larger images and models must use
multipart transfers. Oversized commands are discarded through their terminator;
payload fragments are never rendered as text. CAN and SUB cancel an incomplete
command. A canceled or discarded command also aborts incomplete graphics transfers.

Kitty transfers accept up to 32 MiB of decoded payload. Images are limited to
8192 pixels per dimension and 64 MiB of RGBA pixels, with limits checked before
image decoding allocates its output. A rejected multipart transfer discards its
continuations through the final chunk; an explicit new transfer can restart it.

RGP permits eight concurrent multipart registrations, at most 32 MiB per model
and 64 MiB across incomplete registrations. Exceeding these limits or changing
format mid-transfer aborts the registrations. Send a delete-all command before
restarting them; this prevents an orphaned final chunk from being mistaken for a
new model. Placement dimensions must be between 1 and 65535 cells.

DEC synchronized output (mode 2026) commits terminal text, graphics operations,
and camera commands together. MCP screen reads observe the committed frame.
An incomplete frame is published after one second, at 2 MiB of held data or
4096 commands, on disconnect, or before a host resize. After a limit or timeout,
output streams normally until another begin-frame sequence. Commands inside an
APC payload cannot terminate a frame. A frame spanning multiple PTY reads has
the same behavior as one received in a single read.

Terminal query replies have a separate 64 KiB / 1024-packet budget per drain;
excess replies are discarded with a diagnostic. Distinct unhandled-sequence
log entries are capped at 128. PTY input accepts at most 1 MiB in 256 packets,
including an in-progress write. A successful MCP input response means
`bytes_accepted` into that queue (`delivery: "queued"`); state observations report
actual `input_written_bytes`, pending bytes, and any writer error. A full queue
returns `input_backpressure` without blocking the rendering thread.

The graphics registry retains at most 64 objects and 256 MiB of raster pixel
payloads. Further registrations are rejected; delete unused objects to make
room. File-based RGP models have the same 32 MiB source-size limit as transfers.
These are parser and registry budgets, not a cap on the GPU driver's allocations
or on textures referenced by a glTF asset.

Configured ANSI colors are supplied to the terminal engine for palette queries.
OSC 4/104 palette overrides and OSC 10/11/12 foreground, background, and cursor
colors affect existing content as well as subsequent output; resetting an
application override restores the configured theme. Default-background opacity
still comes from the window configuration. OSC 17/19 can override selection
colors. MCP snapshots retain their `default` and `indexed:N` style names and
include a `colors` table giving their effective RGB values.
