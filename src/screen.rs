//! Ratty's view of the terminal screen.
//!
//! [`fux_vt`] models the screen and its history but leaves the scrollback
//! offset to its host: reading history is a window onto it, never a change
//! to the terminal. [`ScreenView`] pairs the screen with the offset ratty
//! keeps, so renderers, selection and inline objects read the rows the user
//! is looking at.

use std::ops::Deref;

use fux_vt::{CellRef, Row, RowId, Screen, Window};

/// A retained text position. Ordinary output follows its row identity. Reflow
/// follows its offset in the soft-wrapped logical line, whose first row keeps
/// its identity. Eviction or a screen switch invalidates the position.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextAnchor {
    row: RowId,
    line: RowId,
    col: u16,
    offset: usize,
    width: u16,
    alternate: bool,
}

impl TextAnchor {
    pub(crate) fn capture(screen: &Screen, row: Row<'_>, col: u16) -> Self {
        let col = if row
            .cell(usize::from(col))
            .is_some_and(|c| c.is_wide_continuation())
        {
            col.saturating_sub(1)
        } else {
            col
        };
        let mut line = row;
        let mut offset = usize::from(col);
        while let Some(previous) = line.above().filter(Row::wrapped) {
            offset = offset.saturating_add(text_width(previous));
            line = previous;
        }
        Self {
            row: row.id(),
            line: line.id(),
            col,
            offset,
            width: screen.size().cols(),
            alternate: screen.mode(fux_vt::Mode::AlternateScreen),
        }
    }

    pub(crate) fn resolve<'a>(&self, screen: &'a Screen) -> Option<(Row<'a>, u16)> {
        if self.alternate != screen.mode(fux_vt::Mode::AlternateScreen) {
            return None;
        }
        if self.width == screen.size().cols() {
            return Some((screen.row_by_id(self.row)?, self.col));
        }
        let mut row = screen.row_by_id(self.line)?;
        let mut offset = self.offset;
        while row.wrapped() && offset >= text_width(row) {
            offset = offset.saturating_sub(text_width(row));
            row = row.below()?;
        }
        let col = u16::try_from(offset.min(row.len().saturating_sub(1))).ok()?;
        Some((row, col))
    }
}

fn text_width(row: Row<'_>) -> usize {
    let spacer = row.wrapped()
        && row
            .len()
            .checked_sub(1)
            .and_then(|col| row.cell(col))
            .is_some_and(|c| !c.has_contents() && c.attributes() == fux_vt::Attributes::default())
        && row
            .below()
            .and_then(|next| next.cell(0))
            .is_some_and(|c| c.is_wide());
    row.len().saturating_sub(usize::from(spacer))
}

/// Kitty graphics Unicode placeholder (U+10EEEE). A cell starting with it
/// marks where an image goes; its diacritics carry the image row and column.
pub const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

/// The terminal screen as the user sees it: scrolled back by
/// [`scrollback`](Self::scrollback) rows into history.
///
/// Dereferences to [`Screen`] for everything that does not depend on the
/// scrollback offset (size, modes, the cursor's logical position).
#[derive(Clone, Copy)]
pub struct ScreenView<'a> {
    screen: &'a Screen,
    scrollback: usize,
    window: Window<'a>,
}

impl<'a> ScreenView<'a> {
    /// A view of `screen` scrolled back `scrollback` rows, clamped to the
    /// history the current screen holds (the alternate screen holds none).
    #[must_use]
    pub fn new(screen: &'a Screen, scrollback: usize) -> Self {
        let scrollback = scrollback.min(screen.history_len());
        let window = screen
            .window()
            .row(0)
            .map_or_else(|| screen.window(), |row| row.up(scrollback).window());
        Self {
            screen,
            scrollback,
            window,
        }
    }

    /// Returns how many rows the view is scrolled back into history.
    #[must_use]
    pub fn scrollback(&self) -> usize {
        self.scrollback
    }

    /// Returns the row drawn at `row`, counting from the top of the view.
    #[must_use]
    pub fn visible_row(&self, row: u16) -> Option<Row<'a>> {
        self.window.row(row)
    }

    /// The resolved window shared by rendering, selection and observations.
    #[must_use]
    pub fn window(&self) -> Window<'a> {
        self.window
    }

    /// Returns the cell drawn at `row`, `col` of the view.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<CellRef<'a>> {
        self.window.cell(row, col)
    }

    /// Returns the cell a renderer should draw the cursor in, as `(row, col)`.
    ///
    /// [`Screen::cursor_position`] reports the logical position, whose column
    /// stays on the grid while a character in the last column waits to
    /// wrap. This clamps that column onto the grid and, when the cursor sits
    /// on the second half of a wide character, moves it to the first half,
    /// so the cell always exists and owns its glyph.
    #[must_use]
    pub fn display_cursor_position(&self) -> (u16, u16) {
        let (row, col) = self.screen.cursor_position();
        let cols = self.screen.size().cols();
        let mut col = col.min(cols.saturating_sub(1));
        if self
            .screen
            .cell(row, col)
            .is_some_and(|cell| cell.is_wide_continuation())
        {
            col = col.saturating_sub(1);
        }
        (row, col)
    }

    /// Returns whether a renderer should leave the cursor undrawn: hidden by
    /// DECTCEM, or the view scrolled back, as the cursor belongs to the live
    /// screen and must not be painted over history.
    #[must_use]
    pub fn cursor_hidden(&self) -> bool {
        !self.screen.mode(fux_vt::Mode::ShowCursor) || self.scrollback > 0
    }

    /// Returns each row of the view as text, trailing blanks trimmed.
    #[must_use]
    pub fn row_texts(&self) -> Vec<String> {
        let rows = self.screen.size().rows();
        (0..rows)
            .map(|row| {
                let mut text = String::new();
                if let Some(row) = self.visible_row(row) {
                    for cell in row.cells().filter(|c| !c.is_wide_continuation()) {
                        text.push_str(if cell.has_contents() {
                            cell.contents()
                        } else {
                            " "
                        });
                    }
                }
                text.truncate(text.trim_end().len());
                text
            })
            .collect()
    }
}

impl Deref for ScreenView<'_> {
    type Target = Screen;

    fn deref(&self) -> &Screen {
        self.screen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fux_vt::Parser;

    fn parse(rows: u16, cols: u16, input: &[u8]) -> Parser {
        let mut parser =
            Parser::new(fux_vt::Size::new(rows, cols).expect("valid size"), 20).expect("parser");
        parser.process(input).expect("process");
        parser
    }

    #[test]
    fn scrolling_back_shows_history_and_clamps_to_it() {
        let mut input = Vec::new();
        for line in 0..12 {
            input.extend_from_slice(format!("row{line}\r\n").as_bytes());
        }
        let parser = parse(3, 10, &input);
        let screen = parser.screen();
        assert_eq!(
            ScreenView::new(screen, 0).row_texts(),
            ["row10", "row11", ""]
        );
        assert_eq!(
            ScreenView::new(screen, 2).row_texts(),
            ["row8", "row9", "row10"]
        );
        let top = ScreenView::new(screen, usize::MAX);
        assert_eq!(top.scrollback(), 10);
        assert_eq!(top.row_texts(), ["row0", "row1", "row2"]);
        assert!(top.visible_row(3).is_none());
        assert_eq!(top.cell(0, 0).map(|cell| cell.contents()), Some("r"));
    }

    #[test]
    fn display_cursor_clamps_a_pending_wrap_and_snaps_off_wide_halves() {
        let parser = parse(2, 4, b"abcd");
        let view = ScreenView::new(parser.screen(), 0);
        assert_eq!(view.cursor_position(), (0, 3));
        assert!(view.pending_wrap());
        assert_eq!(view.display_cursor_position(), (0, 3));

        let parser = parse(2, 6, "\u{4f60}\x1b[1;2H".as_bytes());
        let view = ScreenView::new(parser.screen(), 0);
        assert_eq!(view.cursor_position(), (0, 1));
        assert_eq!(view.display_cursor_position(), (0, 0));
    }

    #[test]
    fn cursor_hides_under_dectcem_and_while_scrolled_back() {
        let mut input = b"\x1b[?25l".to_vec();
        let parser_hidden = parse(3, 20, &input);
        assert!(ScreenView::new(parser_hidden.screen(), 0).cursor_hidden());
        input.clear();
        for line in 0..10 {
            input.extend_from_slice(format!("row{line}\r\n").as_bytes());
        }
        let parser = parse(3, 20, &input);
        assert!(!ScreenView::new(parser.screen(), 0).cursor_hidden());
        assert!(ScreenView::new(parser.screen(), 3).cursor_hidden());
    }
}
