//! Terminal surface rendering and Ratatui integration.

use std::fs;
use std::num::NonZeroU16;
use std::path::Path;

use anyhow::Context;
use bevy::prelude::*;
use bevy_terminal_ratatui::RatatuiTerminal;
use bevy_terminal_ratatui::prelude::{
    BlinkConfig, CellSizing, CursorConfig, CursorStyle, FontFaces, FontSizing, FontSource,
    RasterConfig, TerminalRenderConfig, TerminalRenderScale, TerminalTexture, TerminalTheme,
    font_family,
};
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color as TuiColor, Modifier, Style};
use ratatui::widgets::Widget;
use read_fonts::{FileRef, TableProvider};
use rio_vt::crosswords::pos::Column;
use rio_vt::crosswords::square::{Square, Wide};
use rio_vt::crosswords::style::{Style as VtStyle, StyleFlags};

use crate::config::{AppConfig, FontConfig, FontStyleConfig, ThemeConfig};
use crate::mouse::TerminalSelection;
use crate::vt::{self, CellColor, VtTerminal};

// rio-vt 0.5.19 has spare style bits but no named blink flags. The pinned fork
// assigns these bits and preserves SGR 5/6/25. Cargo packages use the unpatched
// registry source, so build.rs disables this mapping for that fallback.
#[cfg(rio_vt_sgr_blink)]
const VT_SLOW_BLINK: StyleFlags = StyleFlags::from_bits_retain(1 << 11);
#[cfg(rio_vt_sgr_blink)]
const VT_RAPID_BLINK: StyleFlags = StyleFlags::from_bits_retain(1 << 12);

/// Terminal grid and presentation dimensions.
#[derive(Clone, Copy, Debug)]
pub struct TerminalLayout {
    /// Terminal column count.
    pub cols: u16,
    /// Terminal row count.
    pub rows: u16,
    /// Physical texture size in pixels.
    pub texture_size: UVec2,
    /// Logical presentation size in Bevy world units.
    pub logical_size: Vec2,
    /// Physical render scale used for the terminal texture.
    pub render_scale: f32,
}

impl TerminalLayout {
    fn new(cols: u16, rows: u16, texture_size: UVec2, render_scale: f32) -> Self {
        Self {
            cols,
            rows,
            texture_size,
            logical_size: texture_logical_size(texture_size, render_scale),
            render_scale,
        }
    }

    /// Returns PTY pixel dimensions clamped to portable-pty's `u16` API.
    pub fn pty_pixels(self) -> UVec2 {
        self.texture_size.min(UVec2::splat(u16::MAX as u32))
    }
}

/// Terminal redraw flag.
#[derive(Resource)]
pub struct TerminalRedrawState {
    needs_redraw: bool,
}

/// Marks the Bevy entity that renders the application's terminal surface.
#[derive(Component)]
pub(crate) struct TerminalRenderTarget;

/// Font faces resolved from the configured system family or explicit files.
#[derive(Resource, Clone)]
pub struct ConfiguredFontFaces {
    pub(crate) faces: FontFaces,
    /// The system family `faces` names (`None` for explicit font files).
    /// Retained so the availability check validates the family actually
    /// pushed to the renderer, which an embedder may build from a different
    /// `FontConfig` than the inserted `AppConfig`.
    pub(crate) system_family: Option<String>,
}

/// Loads explicit font files into Bevy, or retains the configured system family.
pub fn load_configured_font_faces(
    app: &mut App,
    font: &FontConfig,
) -> anyhow::Result<ConfiguredFontFaces> {
    let explicit = [
        font.regular.as_deref(),
        font.bold.as_deref(),
        font.italic.as_deref(),
        font.bold_italic.as_deref(),
    ];
    if explicit.iter().all(Option::is_none) {
        return Ok(ConfiguredFontFaces {
            faces: FontFaces::regular(font_family(&font.family)),
            system_family: Some(font.family.clone()),
        });
    }

    let regular = font
        .regular
        .as_deref()
        .context("font.regular is required when explicit font files are configured")?;
    let regular = read_font_face(regular)?;
    let bold = font.bold.as_deref().map(read_font_face).transpose()?;
    let italic = font.italic.as_deref().map(read_font_face).transpose()?;
    let bold_italic = font
        .bold_italic
        .as_deref()
        .map(read_font_face)
        .transpose()?;

    let mut fonts = app.world_mut().resource_mut::<Assets<Font>>();
    let regular = fonts.add(regular);
    let bold = bold.map(|font| fonts.add(font));
    let italic = italic.map(|font| fonts.add(font));
    let bold_italic = bold_italic.map(|font| fonts.add(font));

    Ok(ConfiguredFontFaces {
        faces: FontFaces {
            regular: FontSource::Handle(regular),
            bold: bold.map(FontSource::Handle),
            italic: italic.map(FontSource::Handle),
            bold_italic: bold_italic.map(FontSource::Handle),
            synthesize: true,
        },
        system_family: None,
    })
}

fn read_font_face(path: &Path) -> anyhow::Result<Font> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read font face {}", path.display()))?;
    validate_font_face(path, &bytes)?;
    Ok(Font::from_bytes(bytes))
}

fn validate_font_face(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let file = FileRef::new(bytes)
        .with_context(|| format!("failed to parse font face {}", path.display()))?;
    let has_character_map = file.fonts().any(|font| {
        font.ok()
            .and_then(|font| font.cmap().ok())
            .is_some_and(|cmap| cmap.best_subtable().is_some())
    });
    anyhow::ensure!(
        has_character_map,
        "font face {} contains no usable character map",
        path.display()
    );
    Ok(())
}

impl Default for TerminalRedrawState {
    fn default() -> Self {
        Self { needs_redraw: true }
    }
}

impl TerminalRedrawState {
    /// Requests a terminal redraw.
    pub fn request(&mut self) {
        self.needs_redraw = true;
    }

    /// Returns whether a redraw was pending.
    pub fn take(&mut self) -> bool {
        std::mem::take(&mut self.needs_redraw)
    }
}

/// Terminal surface and render state.
#[derive(Resource)]
pub struct TerminalSurface {
    /// Ratatui terminal backend.
    pub tui: RatatuiTerminal,
    /// Front texture image handle (sampled by the plane material and sprite).
    pub image_handle: Option<Handle<Image>>,
    /// Back texture image handle.
    pub back_image_handle: Option<Handle<Image>>,
    /// Terminal column count.
    pub cols: u16,
    /// Terminal row count.
    pub rows: u16,
    cursor_model_visible: bool,
    font_size: i32,
    render_config: TerminalRenderConfig,
    render_scale: f32,
    cell_size: Vec2,
    rendered_texture_size: Option<UVec2>,
}

impl TerminalSurface {
    /// Creates a terminal surface from the application config.
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal backend cannot be initialized.
    pub fn new(config: &AppConfig) -> anyhow::Result<Self> {
        let cols = config.terminal.default_cols;
        let rows = config.terminal.default_rows;
        let (mut tui, _) = RatatuiTerminal::new(cols, rows);
        let _ = tui.clear();
        if config.cursor.model.visible {
            tui.hide_cursor()?;
        } else {
            tui.show_cursor()?;
        }
        // The real scale arrives with the first `resize_to_fit` once the
        // window exists; an explicit override seeds it early.
        let render_scale = config.window.scale_factor.unwrap_or(1.0).max(1.0);
        let render_config = build_terminal_render_config(
            &config.font,
            &config.theme,
            config.window.opacity,
            render_scale,
        );

        Ok(Self {
            tui,
            image_handle: None,
            back_image_handle: None,
            cols,
            rows,
            cursor_model_visible: config.cursor.model.visible,
            font_size: config.font.size,
            render_config,
            render_scale,
            // No geometry is inferred before the renderer measures the loaded font.
            cell_size: Vec2::ONE,
            rendered_texture_size: None,
        })
    }

    /// Adjusts the font size.
    pub fn adjust_font_size(&mut self, delta: i32) -> bool {
        let new_size = self.font_size.saturating_add(delta).max(1);
        if new_size == self.font_size {
            return false;
        }

        self.render_config.font_size = FontSizing::Px(points_to_logical_pixels(new_size));
        self.font_size = new_size;
        true
    }

    /// Returns the current font size.
    pub fn font_size(&self) -> i32 {
        self.font_size
    }

    /// Updates the physical render scale.
    pub(crate) fn set_render_scale(&mut self, render_scale: f32) -> bool {
        let render_scale = render_scale.max(1.0);
        if (render_scale - self.render_scale).abs() < f32::EPSILON {
            return false;
        }

        self.render_scale = render_scale;
        self.render_config.raster.scale = TerminalRenderScale::Fixed(render_scale);
        true
    }

    /// Resizes the terminal grid to fit a logical window size.
    pub fn resize_to_fit(&mut self, logical_size: Vec2, render_scale: f32) -> TerminalLayout {
        self.set_render_scale(render_scale);

        // The renderer sizes its texture with the same exported helper, so
        // the PTY grid and the rendered grid cannot disagree by a cell.
        let grid =
            bevy_terminal_ratatui::render::grid_for(logical_size.max(Vec2::ONE), self.cell_size);
        let (cols, rows) = (grid.width, grid.height);
        if cols != self.cols || rows != self.rows {
            self.resize(cols, rows);
        }

        self.layout()
    }

    /// Resizes the terminal grid.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }

        self.tui.resize_grid(cols, rows);
        if self.cursor_model_visible {
            let _ = self.tui.hide_cursor();
        } else {
            let _ = self.tui.show_cursor();
        }
        self.cols = cols;
        self.rows = rows;
    }

    /// Returns the rendered cell size in logical pixels.
    ///
    /// Always at least 1x1: every writer of `cell_size` (the constructor and
    /// `update_render_output`) floors it, so consumers need no re-clamp.
    pub fn char_dimensions(&self) -> Vec2 {
        self.cell_size
    }

    /// Whether the renderer has supplied authoritative font and cell metrics.
    pub fn is_measured(&self) -> bool {
        self.rendered_texture_size.is_some()
    }

    /// Returns the terminal pixmap dimensions in pixels.
    pub fn pixmap_dimensions(&self) -> UVec2 {
        (Vec2::new(self.cols as f32, self.rows as f32) * self.cell_size * self.render_scale)
            .round()
            .max(Vec2::ONE)
            .as_uvec2()
    }

    /// Returns the current terminal layout.
    pub(crate) fn layout(&self) -> TerminalLayout {
        TerminalLayout::new(
            self.cols,
            self.rows,
            self.pixmap_dimensions(),
            self.render_scale,
        )
    }

    /// Returns the renderer component configuration for the current settings.
    pub(crate) const fn render_config(&self) -> &TerminalRenderConfig {
        &self.render_config
    }

    /// Adopts the metrics and stable image handle produced by the Bevy
    /// renderer, comparing first and writing only on change; returns whether
    /// anything changed. One comparator, no call-site protocol — nothing in
    /// the crate reads `TerminalSurface` change ticks, so an occasional
    /// no-op write would only cost the comparison this method already does.
    pub(crate) fn update_render_output(&mut self, texture: &TerminalTexture) -> bool {
        let (cell_size, render_scale) = clamped_render_metrics(texture);
        let changed = self.image_handle.as_ref() != Some(&texture.image)
            || self.rendered_texture_size != Some(texture.size)
            || self.cell_size != cell_size
            || self.render_scale != render_scale;
        if changed {
            self.image_handle = Some(texture.image.clone());
            self.rendered_texture_size = Some(texture.size);
            self.cell_size = cell_size;
            self.render_scale = render_scale;
        }
        changed
    }
}

/// Floors renderer metrics to the >=1.0 invariant at the single adoption
/// boundary, so a sub-1.0 input can neither be stored nor latch change
/// detection.
fn clamped_render_metrics(texture: &TerminalTexture) -> (Vec2, f32) {
    (
        texture.cell_size.max(Vec2::ONE),
        texture.raster_scale.max(1.0),
    )
}

/// Computes the physical render scale for a Bevy window.
///
/// Delegates to the renderer's exported helper so the scale the PTY layout
/// uses is the exact scale the renderer rasterizes with.
pub fn render_scale_for_window(window: &Window) -> f32 {
    bevy_terminal_ratatui::render::raster_scale_for_window(window)
}

/// Returns the logical size for a physical terminal texture.
pub fn texture_logical_size(texture_size: UVec2, render_scale: f32) -> Vec2 {
    texture_size.as_vec2() / render_scale.max(1.0)
}

fn build_terminal_render_config(
    font: &FontConfig,
    theme_config: &ThemeConfig,
    window_opacity: f32,
    render_scale: f32,
) -> TerminalRenderConfig {
    let [fg_r, fg_g, fg_b] = theme_config.foreground;
    let [bg_r, bg_g, bg_b] = theme_config.background;
    let [cursor_r, cursor_g, cursor_b] = theme_config.cursor;
    let theme = TerminalTheme {
        foreground: Color::srgb_u8(fg_r, fg_g, fg_b),
        background: Color::srgba_u8(
            bg_r,
            bg_g,
            bg_b,
            (window_opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
        ),
        ansi: theme_config
            .palette()
            .map(|[r, g, b]| Color::srgb_u8(r, g, b)),
    };

    TerminalRenderConfig {
        // Cell width and height come from the loaded face's measured advance
        // and line box; Ratty supplies no independent geometry estimate.
        cell_size: CellSizing::FROM_FONT,
        font: FontFaces::regular(font_family(&font.family)),
        font_size: FontSizing::Px(points_to_logical_pixels(font.size)),
        theme,
        cursor: CursorConfig {
            style: CursorStyle::Block,
            color: Color::srgb_u8(cursor_r, cursor_g, cursor_b),
            blink_hz: None,
        },
        blink: BlinkConfig {
            slow_hz: Some(1.0),
            rapid_hz: Some(2.0),
        },
        raster: RasterConfig {
            scale: TerminalRenderScale::Fixed(render_scale.max(1.0)),
            ..default()
        },
    }
}

fn points_to_logical_pixels(points: i32) -> f32 {
    // A typographic point is defined as 1/72 inch, while a logical/CSS pixel
    // is defined as 1/96 inch. These constants convert units; they do not
    // estimate any property of the selected font.
    const POINTS_PER_INCH: f32 = 72.0;
    const LOGICAL_PIXELS_PER_INCH: f32 = 96.0;
    (points as f32 * LOGICAL_PIXELS_PER_INCH / POINTS_PER_INCH).max(1.0)
}

/// Ratatui widget backed by the rio-vt grid.
pub struct TerminalWidget<'a> {
    /// Terminal state to render.
    pub term: &'a VtTerminal,
    /// Active selection.
    pub selection: &'a TerminalSelection,
    /// Terminal theme.
    pub theme: &'a ThemeConfig,
    /// Base font style override.
    pub font_style: FontStyleConfig,
}

impl Widget for TerminalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [fg_r, fg_g, fg_b] = self.theme.foreground;
        let theme_fg = TuiColor::Rgb(fg_r, fg_g, fg_b);
        let theme_palette = self.theme.palette().map(|[r, g, b]| TuiColor::Rgb(r, g, b));
        buf.set_style(area, Style::default().fg(theme_fg));

        let selection = self.selection.normalized_bounds();
        let rows = u16::try_from(self.term.screen_lines()).unwrap_or(u16::MAX);
        let cols = u16::try_from(self.term.columns()).unwrap_or(u16::MAX);
        let draw_rows = rows.min(area.height);
        let draw_cols = cols.min(area.width);
        let styles = vt::styles(self.term);
        let mut symbol = String::new();

        for row in 0..draw_rows {
            let Some(grid_row) = vt::visible_row(self.term, row) else {
                continue;
            };
            for col in 0..draw_cols {
                let square = grid_row[Column(usize::from(col))];
                let cell = &mut buf[(area.x + col, area.y + row)];

                // When a Spacer trails a wide anchor, ratatui's backend diff
                // skips it and the renderer copies the anchor's style, so the
                // anchor branch below owns that highlight. But rio-vt can
                // orphan a spacer (e.g. ESC[1K blanks the cells before it
                // without touching it); behind a narrow predecessor the diff
                // does emit this cell, so style it fully — including its own
                // selection highlight.
                if matches!(square.wide(), Wide::Spacer) {
                    let mut style =
                        square_style(styles, square, &theme_palette, theme_fg, self.font_style);
                    if selection.is_some_and(|bounds| bounds.contains(row, col)) {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    cell.set_symbol(" ")
                        .set_style(style)
                        .set_diff_option(forced_width(1));
                    continue;
                }

                // A leading spacer is the end-of-line pad Rio writes before a
                // wide glyph wraps. It is not part of the glyph and must remain
                // an unstyled blank so the normal diff clears content left by
                // a previous frame.
                if matches!(square.wide(), Wide::LeadingSpacer) {
                    cell.set_symbol(" ").set_diff_option(forced_width(1));
                    continue;
                }

                symbol.clear();
                vt::push_cell_text(
                    &mut symbol,
                    &self.term.grid,
                    vt::visible_pos(self.term, row, col),
                );

                let mut style =
                    square_style(styles, square, &theme_palette, theme_fg, self.font_style);
                let is_wide = matches!(square.wide(), Wide::Wide);
                // A wide glyph renders as one unit: a selection touching
                // either of its columns highlights the whole glyph, since the
                // trailing spacer cannot carry its own style (see above).
                if selection.is_some_and(|bounds| {
                    bounds.contains(row, col)
                        || (is_wide && col + 1 < cols && bounds.contains(row, col + 1))
                }) {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                let width = if is_wide { 2 } else { 1 };
                cell.set_symbol(if symbol.is_empty() { " " } else { &symbol })
                    .set_style(style)
                    .set_diff_option(forced_width(width));
            }
        }
    }
}

fn forced_width(width: u16) -> CellDiffOption {
    CellDiffOption::ForcedWidth(NonZeroU16::new(width).expect("terminal cell width is non-zero"))
}

fn square_style(
    styles: &[VtStyle],
    square: Square,
    theme_palette: &[TuiColor; 16],
    theme_fg: TuiColor,
    font_style: FontStyleConfig,
) -> Style {
    let (fg, bg, underline, flags) = vt::cell_attributes(styles, square);

    let mut style = Style::default().fg(cell_color_to_tui(fg, theme_palette).unwrap_or(theme_fg));
    if let Some(bg) = cell_color_to_tui(bg, theme_palette) {
        style = style.bg(bg);
    }
    if let Some(underline) = underline.and_then(|color| cell_color_to_tui(color, theme_palette)) {
        style = style.underline_color(underline);
    }

    let mut modifiers = match font_style {
        FontStyleConfig::Regular => Modifier::empty(),
        FontStyleConfig::Bold => Modifier::BOLD,
        FontStyleConfig::Italic => Modifier::ITALIC,
        FontStyleConfig::BoldItalic => Modifier::BOLD | Modifier::ITALIC,
    };
    if flags.contains(StyleFlags::BOLD) {
        modifiers |= Modifier::BOLD;
    }
    if flags.contains(StyleFlags::DIM) {
        modifiers |= Modifier::DIM;
    }
    if flags.contains(StyleFlags::ITALIC) {
        modifiers |= Modifier::ITALIC;
    }
    // Ratatui has a single underline modifier, so every underline style
    // rio-vt distinguishes (straight, double, curly, dotted, dashed) collapses
    // onto it.
    if flags.intersects(StyleFlags::ALL_UNDERLINES) {
        modifiers |= Modifier::UNDERLINED;
    }
    if flags.contains(StyleFlags::INVERSE) {
        modifiers |= Modifier::REVERSED;
    }
    if flags.contains(StyleFlags::HIDDEN) {
        modifiers |= Modifier::HIDDEN;
    }
    if flags.contains(StyleFlags::STRIKEOUT) {
        modifiers |= Modifier::CROSSED_OUT;
    }
    #[cfg(rio_vt_sgr_blink)]
    {
        if flags.contains(VT_SLOW_BLINK) {
            modifiers |= Modifier::SLOW_BLINK;
        }
        if flags.contains(VT_RAPID_BLINK) {
            modifiers |= Modifier::RAPID_BLINK;
        }
    }

    style = style.add_modifier(modifiers);
    style
}

fn cell_color_to_tui(color: CellColor, theme_palette: &[TuiColor; 16]) -> Option<TuiColor> {
    match color {
        CellColor::Default => None,
        CellColor::Indexed(index) => Some(ansi_index_to_tui(index, theme_palette)),
        CellColor::Rgb(r, g, b) => Some(TuiColor::Rgb(r, g, b)),
    }
}

fn ansi_index_to_tui(index: u8, theme_palette: &[TuiColor; 16]) -> TuiColor {
    match index {
        0..=15 => theme_palette[index as usize],
        16..=231 => {
            let index = index - 16;
            let r = index / 36;
            let g = (index % 36) / 6;
            let b = index % 6;
            let component = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
            TuiColor::Rgb(component(r), component(g), component(b))
        }
        232..=255 => {
            let shade = 8 + (index - 232) * 10;
            TuiColor::Rgb(shade, shade, shade)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bevy_terminal_ratatui::prelude::TerminalColor;
    use ratatui::buffer::Cell;
    use rio_vt::ansi::CursorShape;
    use rio_vt::crosswords::{Crosswords, CrosswordsSize};
    use rio_vt::event::WindowId;
    use rio_vt::performer::handler::Processor;

    use crate::vt::TerminalEventSink;

    /// Renders `input` through [`TerminalWidget`] and returns row 0's cells.
    fn render_cells(rows: u16, cols: u16, input: &[u8]) -> Vec<Cell> {
        let mut term = Crosswords::new(
            CrosswordsSize::new(usize::from(cols), usize::from(rows)),
            CursorShape::Block,
            TerminalEventSink::default(),
            WindowId::from(0),
            0,
            1000,
        );
        Processor::default().advance(&mut term, input);

        let area = Rect::new(0, 0, cols, rows);
        let mut buffer = Buffer::empty(area);
        TerminalWidget {
            term: &term,
            selection: &TerminalSelection::default(),
            theme: &ThemeConfig::default(),
            font_style: FontStyleConfig::Regular,
        }
        .render(area, &mut buffer);

        (0..cols).map(|col| buffer[(col, 0)].clone()).collect()
    }

    /// Renders `input` through [`TerminalWidget`] and returns the drawn rows.
    fn render_rows(rows: u16, cols: u16, input: &[u8]) -> Vec<String> {
        let mut term = Crosswords::new(
            CrosswordsSize::new(usize::from(cols), usize::from(rows)),
            CursorShape::Block,
            TerminalEventSink::default(),
            WindowId::from(0),
            0,
            1000,
        );
        Processor::default().advance(&mut term, input);

        let area = Rect::new(0, 0, cols, rows);
        let mut buffer = Buffer::empty(area);
        TerminalWidget {
            term: &term,
            selection: &TerminalSelection::default(),
            theme: &ThemeConfig::default(),
            font_style: FontStyleConfig::Regular,
        }
        .render(area, &mut buffer);

        (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| buffer[(col, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Draws a terminal state through Ratatui's differential update path into
    /// the retained Bevy terminal surface.
    fn draw_term(tui: &mut RatatuiTerminal, term: &Crosswords<TerminalEventSink>) {
        tui.draw(|frame| {
            frame.render_widget(
                TerminalWidget {
                    term,
                    selection: &TerminalSelection::default(),
                    theme: &ThemeConfig::default(),
                    font_style: FontStyleConfig::Regular,
                },
                frame.area(),
            );
        });
    }

    /// Builds and draws a fresh terminal state through [`draw_term`].
    fn draw_input(tui: &mut RatatuiTerminal, rows: u16, cols: u16, input: &[u8]) {
        let mut term = Crosswords::new(
            CrosswordsSize::new(usize::from(cols), usize::from(rows)),
            CursorShape::Block,
            TerminalEventSink::default(),
            WindowId::from(0),
            0,
            1000,
        );
        Processor::default().advance(&mut term, input);
        draw_term(tui, &term);
    }

    /// Builds a headless Bevy text app that measures Ratty's renderer config.
    fn measured_terminal_app(font_size: i32, render_scale: f32) -> (App, Entity) {
        let app_config = AppConfig {
            font: FontConfig {
                family: "Ratty Definitely Missing Mono".to_string(),
                size: font_size,
                ..default()
            },
            window: crate::config::WindowConfig {
                scale_factor: Some(render_scale),
                ..default()
            },
            ..default()
        };
        let terminal = TerminalSurface::new(&app_config).expect("terminal");
        let render_surface = terminal.tui.surface();
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            bevy::asset::AssetPlugin::default(),
            bevy::text::TextPlugin,
            bevy_terminal_ratatui::prelude::TerminalPlugin,
        ))
        .init_asset::<Image>()
        .insert_resource(app_config)
        .insert_resource(terminal)
        .add_systems(
            Update,
            crate::systems::sync_terminal_renderer_config
                .before(bevy_terminal_ratatui::prelude::TerminalSystems::Sync),
        );
        let entity = app
            .world_mut()
            .spawn((
                TerminalRenderTarget,
                bevy_terminal_ratatui::TerminalRenderer::new(render_surface),
            ))
            .id();
        for _ in 0..4 {
            app.update();
        }
        (app, entity)
    }

    /// End-to-end guard for the defect that motivated this engine port: older
    /// rio-vt releases bound `visible_rows` to the DECSTBM scroll region, so a
    /// widget built on it drew the grid shifted up with blank rows at the bottom
    /// the moment an application narrowed the region.
    #[test]
    fn widget_draws_every_row_with_a_scroll_region_set() {
        let mut input = Vec::new();
        for line in 0..8 {
            input.extend_from_slice(format!("\x1b[{};1Hline{line}", line + 1).as_bytes());
        }
        let without_region = render_rows(8, 20, &input);

        input.extend_from_slice(b"\x1b[2;6r");
        let with_region = render_rows(8, 20, &input);

        let expected: Vec<String> = (0..8).map(|line| format!("line{line}")).collect();
        assert_eq!(without_region, expected);
        assert_eq!(
            with_region, expected,
            "a narrowed scroll region must not shift or blank the drawn grid"
        );
    }

    /// When a wide glyph does not fit, rio-vt pads end-of-line with a space
    /// carrying the active SGR style. Drawing that pad paints a styled block at
    /// the line end and breaks the renderer's wide-cell heuristic, which only
    /// treats a trailing space as a continuation when it has no background.
    /// vt100 had no equivalent cell, so this guards buffer parity with it.
    #[test]
    fn wrapped_wide_characters_leave_an_unstyled_pad() {
        // Green background, then thirteen narrow cells so the wide glyph cannot
        // fit in the one remaining column and Rio writes a LeadingSpacer there.
        let rendered = render_cells(2, 14, b"\x1b[42mabcdefghijklm\xe4\xbd\xa0\x1b[0m");

        let pad = &rendered[13];
        assert_eq!(pad.symbol(), " ", "the pad cell must stay blank");
        assert_eq!(
            pad.bg,
            TuiColor::Reset,
            "the pad cell must not carry the active background"
        );
    }

    #[test]
    fn widget_preserves_hidden_strikeout_and_underline_color() {
        let rendered = render_cells(1, 3, b"\x1b[4;8;9;58;2;1;2;3mX");
        let cell = &rendered[0];

        assert!(cell.modifier.contains(Modifier::UNDERLINED));
        assert!(cell.modifier.contains(Modifier::HIDDEN));
        assert!(cell.modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(cell.underline_color, TuiColor::Rgb(1, 2, 3));
    }

    #[cfg(rio_vt_sgr_blink)]
    #[test]
    fn widget_preserves_slow_and_rapid_blink() {
        let rendered = render_cells(1, 3, b"\x1b[5mS\x1b[6mR\x1b[25mN");

        assert!(rendered[0].modifier.contains(Modifier::SLOW_BLINK));
        assert!(!rendered[0].modifier.contains(Modifier::RAPID_BLINK));
        assert!(!rendered[1].modifier.contains(Modifier::SLOW_BLINK));
        assert!(rendered[1].modifier.contains(Modifier::RAPID_BLINK));
        assert!(
            !rendered[2]
                .modifier
                .intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK)
        );
    }

    #[test]
    fn widget_forces_rio_vt_cell_widths() {
        let rendered = render_cells(1, 4, "A你".as_bytes());

        assert_eq!(rendered[0].diff_option, forced_width(1));
        assert_eq!(rendered[1].diff_option, forced_width(2));
        assert_eq!(rendered[2].diff_option, forced_width(1));
    }

    /// Ratatui skips a wide glyph's second cell when sending a diff to a real
    /// terminal. The retained backend must reconstruct the skipped continuation
    /// with the wide anchor's style so stale content cannot survive.
    #[test]
    fn successive_draws_replace_wide_continuation_cells() {
        let (rows, cols) = (2, 8);
        let (mut tui, _) = RatatuiTerminal::new(cols, rows);

        draw_input(&mut tui, rows, cols, b"abcdefgh");
        draw_input(
            &mut tui,
            rows,
            cols,
            "\x1b[42m\u{4f60}\u{1f600}\x1b[0m".as_bytes(),
        );

        let buffer = tui.snapshot();
        assert_eq!(buffer[(0, 0)].symbol(), "\u{4f60}");
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), "\u{1f600}");
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert!(buffer[(1, 0)].is_continuation());
        assert!(buffer[(3, 0)].is_continuation());
        assert_eq!(buffer[(1, 0)].style, buffer[(0, 0)].style);
        assert_eq!(buffer[(3, 0)].style, buffer[(2, 0)].style);
        assert_ne!(buffer[(0, 0)].style.background, TerminalColor::Default);
        for col in 4..cols {
            assert_eq!(
                buffer[(col, 0)].symbol(),
                " ",
                "old content survived at column {col}"
            );
        }
    }

    /// Repeatedly moving wide graphemes into and out of the viewport must not
    /// leave their owners or continuation cells behind on unrelated rows.
    #[test]
    fn scrollback_redraws_wide_graphemes_without_artifacts() {
        let (rows, cols) = (2, 8);
        let (mut tui, _) = RatatuiTerminal::new(cols, rows);
        let mut term = Crosswords::new(
            CrosswordsSize::new(usize::from(cols), usize::from(rows)),
            CursorShape::Block,
            TerminalEventSink::default(),
            WindowId::from(0),
            0,
            1000,
        );
        Processor::default().advance(
            &mut term,
            "\x1b[42m\u{4f60}\u{1f600}\x1b[0m\r\nsecond\r\nthird\r\nfourth".as_bytes(),
        );

        for offset in [1, 2, 0, 2, 1, 2] {
            crate::vt::set_scrollback(&mut term, offset);
            draw_term(&mut tui, &term);

            let buffer = tui.snapshot();
            if offset == 2 {
                assert_eq!(buffer[(0, 0)].symbol(), "\u{4f60}");
                assert_eq!(buffer[(1, 0)].symbol(), " ");
                assert_eq!(buffer[(2, 0)].symbol(), "\u{1f600}");
                assert_eq!(buffer[(3, 0)].symbol(), " ");
                assert!(buffer[(1, 0)].is_continuation());
                assert!(buffer[(3, 0)].is_continuation());
                assert_eq!(buffer[(1, 0)].style, buffer[(0, 0)].style);
                assert_eq!(buffer[(3, 0)].style, buffer[(2, 0)].style);
            } else {
                assert!(
                    buffer
                        .cells()
                        .iter()
                        .all(|cell| !matches!(cell.symbol(), "\u{4f60}" | "\u{1f600}"))
                );
            }
        }
    }

    /// Earlier rio-vt releases aborted placing a double-width glyph in a
    /// single-column grid, which is why the grid floor used to be two columns.
    /// Window managers and drag-resize routinely produce very narrow grids, so
    /// keep a guard on the sizes that used to panic.
    #[test]
    fn degenerate_grids_render_without_panicking() {
        for (rows, cols) in [(1_u16, 1_u16), (1, 40), (40, 1), (2, 2)] {
            let rendered = render_rows(rows, cols, "\u{4f60}\u{597d}ab".as_bytes());
            assert_eq!(rendered.len(), usize::from(rows));
        }
    }

    #[test]
    fn widget_draws_wide_characters_and_combining_marks() {
        let rendered = render_rows(2, 10, "你好e\u{0301}z".as_bytes());

        // The spacer cell after a wide glyph stays blank rather than repeating it.
        assert_eq!(rendered[0], "你 好 e\u{0301}z");
    }

    /// Bevy snaps cell dimensions to whole physical pixels. Adjacent requested
    /// sizes can therefore share one effective font/cell at low DPI, but the
    /// sequence may never shrink and the full zoom range must grow both axes.
    #[test]
    fn font_size_steps_change_measured_cells_without_shrinking() {
        for render_scale in [1.0, 2.0] {
            let (mut app, entity) = measured_terminal_app(8, render_scale);
            let mut previous = app
                .world()
                .get::<TerminalTexture>(entity)
                .expect("initial measured texture")
                .cell_size;
            let initial = previous;
            for size in 9..=24 {
                assert!(
                    app.world_mut()
                        .resource_mut::<TerminalSurface>()
                        .adjust_font_size(1)
                );
                app.update();
                let texture = app
                    .world()
                    .get::<TerminalTexture>(entity)
                    .expect("remeasured texture")
                    .clone();
                let requested = points_to_logical_pixels(size);
                assert_eq!(
                    app.world()
                        .resource::<TerminalSurface>()
                        .render_config()
                        .font_size,
                    FontSizing::Px(requested)
                );
                assert!(texture.font_size.is_finite() && texture.font_size >= 1.0);
                let measured = texture.cell_size;
                assert!(
                    measured.cmpge(previous).all(),
                    "cell shrank at size {size} (scale {render_scale}): \
                     {previous:?} -> {measured:?}"
                );
                previous = measured;
            }
            assert!(
                previous.cmpgt(initial).all(),
                "zoom range did not grow both axes at scale {render_scale}: \
                 {initial:?} -> {previous:?}"
            );
        }
    }

    #[test]
    fn unavailable_font_falls_back_and_adopts_measured_metrics() {
        let (mut app, entity) = measured_terminal_app(12, 1.0);

        let render_config = app
            .world()
            .get::<TerminalRenderConfig>(entity)
            .expect("render config");
        assert_eq!(render_config.font.regular, FontSource::Monospace);
        #[cfg(bevy_terminal_automatic_metrics)]
        {
            assert_eq!(render_config.cell_size, CellSizing::FROM_FONT);
            assert!(matches!(render_config.font_size, FontSizing::Px(_)));
        }
        #[cfg(not(bevy_terminal_automatic_metrics))]
        {
            let CellSizing::Logical(cell) = render_config.cell_size else {
                panic!("packaged-source cells must use the measured-cell adapter");
            };
            assert!(cell.x > 1.0);
            assert!(cell.y > 1.0);
            let FontSizing::Px(font_size) = render_config.font_size else {
                panic!("packaged-source font size must be measured in pixels");
            };
            assert!(font_size.is_finite() && font_size >= 1.0);
        }
        let texture = app
            .world()
            .get::<TerminalTexture>(entity)
            .expect("measured terminal texture")
            .clone();
        assert!(texture.cell_size.cmpgt(Vec2::ONE).all());
        assert!(texture.cell_size.y >= points_to_logical_pixels(12));

        let cell_size = texture.cell_size;
        let mut terminal = app.world_mut().resource_mut::<TerminalSurface>();
        assert!(terminal.update_render_output(&texture));
        let layout = terminal.resize_to_fit(cell_size * Vec2::new(4.9, 3.9), 1.0);
        assert_eq!((layout.cols, layout.rows), (4, 3));
    }

    #[test]
    fn font_config_always_uses_measured_cell_metrics() {
        let config = AppConfig {
            font: FontConfig {
                size: 20,
                ..default()
            },
            window: crate::config::WindowConfig {
                scale_factor: Some(2.0),
                ..default()
            },
            ..default()
        };
        let mut terminal = TerminalSurface::new(&config).expect("measured terminal");
        assert_eq!(terminal.render_config().cell_size, CellSizing::FROM_FONT);
        assert_eq!(
            terminal.render_config().font_size,
            FontSizing::Px(points_to_logical_pixels(20))
        );
        assert_eq!(
            terminal.render_config().raster.scale,
            TerminalRenderScale::Fixed(2.0)
        );

        let unmeasured_cell = terminal.char_dimensions();
        assert!(terminal.adjust_font_size(2));
        assert_eq!(terminal.render_config().cell_size, CellSizing::FROM_FONT);
        assert_eq!(
            terminal.render_config().font_size,
            FontSizing::Px(points_to_logical_pixels(22))
        );
        assert_eq!(
            terminal.char_dimensions(),
            unmeasured_cell,
            "zoom must retain the last authoritative metrics until remeasurement"
        );
    }

    #[test]
    fn explicit_face_configuration_requires_a_regular_face() {
        let font = FontConfig {
            bold: Some("Bold.ttf".into()),
            ..default()
        };
        let Err(error) = load_configured_font_faces(&mut App::new(), &font) else {
            panic!("bold without regular must fail");
        };

        assert!(error.to_string().contains("font.regular is required"));
    }

    #[test]
    fn invalid_explicit_font_data_is_rejected_before_asset_loading() {
        let font = FontConfig {
            regular: Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")),
            ..default()
        };
        let Err(error) = load_configured_font_faces(&mut App::new(), &font) else {
            panic!("non-font data must fail");
        };

        assert!(error.to_string().contains("failed to parse font face"));
    }

    #[test]
    fn explicit_font_without_a_character_map_is_rejected() {
        let empty_sfnt = [
            0x00, 0x01, 0x00, 0x00, // TrueType scaler type
            0x00, 0x00, // no tables
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, 0x00, // range shift
        ];
        let error = validate_font_face(Path::new("empty.ttf"), &empty_sfnt)
            .expect_err("a font without a character map must fail");

        assert!(error.to_string().contains("no usable character map"));
    }

    #[test]
    fn renderer_output_only_changes_when_presentation_metrics_change() {
        let mut terminal = TerminalSurface::new(&AppConfig::default()).expect("terminal");
        let mut texture = TerminalTexture {
            image: Handle::default(),
            size: UVec2::new(800, 600),
            logical_size: Vec2::new(800.0, 600.0),
            raster_scale: 1.0,
            cell_size: Vec2::new(12.0, 24.0),
            font_size: 20.0,
        };

        assert!(terminal.update_render_output(&texture));
        assert!(!terminal.update_render_output(&texture));

        texture.size.x += 12;
        assert!(terminal.update_render_output(&texture));
    }

    #[test]
    fn sub_pixel_cell_metrics_do_not_latch_renderer_output_changes() {
        let mut terminal = TerminalSurface::new(&AppConfig::default()).expect("terminal");
        let texture = TerminalTexture {
            image: Handle::default(),
            size: UVec2::new(800, 600),
            logical_size: Vec2::new(800.0, 600.0),
            raster_scale: 0.5,
            cell_size: Vec2::new(0.5, 0.75),
            font_size: 1.0,
        };

        assert!(terminal.update_render_output(&texture));
        assert!(
            !terminal.update_render_output(&texture),
            "clamped metrics must compare equal on the second adoption"
        );
    }

    #[test]
    fn dpi_change_retains_metrics_until_the_renderer_rerasterizes() {
        let mut terminal = TerminalSurface::new(&AppConfig::default()).expect("terminal");
        let texture = TerminalTexture {
            image: Handle::default(),
            size: UVec2::new(800, 600),
            logical_size: Vec2::new(800.0, 600.0),
            raster_scale: 1.0,
            cell_size: Vec2::new(12.0, 24.0),
            font_size: 20.0,
        };
        assert!(terminal.update_render_output(&texture));

        assert!(terminal.set_render_scale(2.0));
        assert_eq!(terminal.char_dimensions(), Vec2::new(12.0, 24.0));
        assert_eq!(
            terminal.render_config().raster.scale,
            TerminalRenderScale::Fixed(2.0)
        );
    }
}
