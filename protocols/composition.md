# Terminal composition

`TerminalSurface::draw` uses a retained `TerminalCompositionCache`. Fux's
structural mark refreshes the mapping from viewport coordinates to persistent
row identities. Row versions determine which cells need composition. Changing
the viewport width, selected columns, resolved palette or base font style also
invalidates the affected rows. Only visible row buffers are retained, so this
cache does not grow with scrollback.

Ratatui resets its destination buffer between frames. The cache copies every
visible row's composed cells into that buffer, including glyph width and style
metadata. Cursor position and visibility are submitted separately on every
draw. Font face/size changes remain renderer configuration changes; the CPU
cells contain symbols rather than rasterized glyphs. Kitty placeholder edits
invalidate rows through their versions, while image/model/camera updates use
the existing scene systems and committed synchronized-frame boundary. Renderer
recreation can reuse composed rows; replacing the terminal parser requires a
new cache because row identities are scoped to its lifetime.

## Measurement

Run on Linux aarch64 (Apple M2 Max), Rust 1.99 development profile with
`CARGO_PROFILE_DEV_OPT_LEVEL=3 CARGO_PROFILE_DEV_DEBUG=0`. Each case is the median
of five independent runs, 500 frames at 120 columns by 40 rows, after seeding
1,000 history lines. PTY startup and history seeding are excluded. Timing covers
host output scanning, VT/APC processing, buffer reset and CPU composition. It
does **not** measure GPU rendering, frame presentation or end-to-end input
latency. These are local measurements, not portable performance guarantees.

| Trace | Full composition before (ms) | Cached composition (ms) | Rows composed, cached / full |
| --- | ---: | ---: | ---: |
| One-character typing | 61.542 | 16.793 | 543 / 20,000 |
| Continuous output, 24 lines/frame | 80.051 | 76.449 | 12,515 / 20,000 |
| Moving through scrollback | 71.871 | 29.023 | 695 / 20,000 |
| CJK, combining marks and emoji | 63.891 | 30.519 | 539 / 20,000 |
| Inline Kitty image updates | 62.358 | 28.220 | 40 / 20,000 |

The full baseline was captured before introducing the cache. Both paths remain
available in `examples/composition_bench.rs` for reproducing the comparison:

```sh
CARGO_PROFILE_DEV_OPT_LEVEL=3 CARGO_PROFILE_DEV_DEBUG=0 cargo run --example composition_bench
CARGO_PROFILE_DEV_OPT_LEVEL=3 CARGO_PROFILE_DEV_DEBUG=0 cargo run --example composition_bench -- --cached
```

Sustained output changes most rows, so the measured benefit there is small.
The inline graphics case retransmits a one-pixel Kitty image through the real
host protocol path; it does not benchmark texture upload or 3D model drawing.

Tests compare the complete cached and full Ratatui buffers byte by byte through
partial control sequences and UTF-8, scroll regions, history, screen switches,
reflow, selections, palettes, font styles and clipped/translated viewports.
Separate checks verify unchanged-row reuse, bounded retention, cursor updates,
and repainting a newly created renderer from the cache.
