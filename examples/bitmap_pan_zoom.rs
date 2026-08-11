use std::{
    env, fs,
    io::{self, Stdout, Write},
};

use anyhow::{Context, Result};
use base64::Engine as _;
use image::GenericImageView as _;
use ratatui::crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode},
    execute, queue,
    style::Print,
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

const BITMAP_ID: u32 = 42;
const PLACEMENT_ID: u32 = 7;
const MAX_BASE64_CHUNK: usize = 4096;
const APC_PREFIX: &str = "\u{1b}_ratty;i;";
const APC_END: &str = "\u{1b}\\";

#[derive(Clone, Copy)]
struct Destination {
    row: u16,
    col: u16,
    columns: u32,
    rows: u32,
}

impl Destination {
    const fn new(row: u16, col: u16, columns: u32, rows: u32) -> Self {
        Self {
            row,
            col,
            columns,
            rows,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Fit {
    Contain,
    Cover,
    Fill,
}

impl Fit {
    const fn protocol_value(self) -> &'static str {
        match self {
            Self::Contain => "contain",
            Self::Cover => "cover",
            Self::Fill => "fill",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Filter {
    Nearest,
    Linear,
}

impl Filter {
    const fn protocol_value(self) -> &'static str {
        match self {
            Self::Nearest => "nearest",
            Self::Linear => "linear",
        }
    }
}

#[derive(Clone, Copy)]
enum Zoom {
    In,
    Out,
}

#[derive(Clone, Copy)]
struct ViewState {
    bitmap_width: u32,
    bitmap_height: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    fit: Fit,
    filter: Filter,
    opacity: f32,
}

impl ViewState {
    fn new(bitmap_width: u32, bitmap_height: u32) -> Self {
        Self {
            bitmap_width,
            bitmap_height,
            x: 0,
            y: 0,
            width: bitmap_width,
            height: bitmap_height,
            fit: Fit::Contain,
            filter: Filter::Linear,
            opacity: 1.0,
        }
    }

    fn pan(&mut self, horizontal: i64, vertical: i64) {
        self.x = offset_clamped(
            self.x,
            horizontal,
            self.bitmap_width.saturating_sub(self.width),
        );
        self.y = offset_clamped(
            self.y,
            vertical,
            self.bitmap_height.saturating_sub(self.height),
        );
    }

    fn zoom(&mut self, direction: Zoom) {
        let (new_width, new_height) = match direction {
            Zoom::In => ((self.width / 2).max(1), (self.height / 2).max(1)),
            Zoom::Out => (
                self.width.saturating_mul(2).min(self.bitmap_width),
                self.height.saturating_mul(2).min(self.bitmap_height),
            ),
        };
        let center_x = self.x.saturating_add(self.width / 2);
        let center_y = self.y.saturating_add(self.height / 2);
        self.width = new_width;
        self.height = new_height;
        self.x = center_x
            .saturating_sub(new_width / 2)
            .min(self.bitmap_width.saturating_sub(new_width));
        self.y = center_y
            .saturating_sub(new_height / 2)
            .min(self.bitmap_height.saturating_sub(new_height));
    }

    fn cycle_fit(&mut self) {
        self.fit = match self.fit {
            Fit::Contain => Fit::Cover,
            Fit::Cover => Fit::Fill,
            Fit::Fill => Fit::Contain,
        };
    }
}

fn offset_clamped(current: u32, delta: i64, maximum: u32) -> u32 {
    let next = i64::from(current).saturating_add(delta);
    next.clamp(0, i64::from(maximum)) as u32
}

fn encode_registration(bitmap_id: u32, encoded_png: &str) -> Vec<Vec<u8>> {
    debug_assert_eq!(MAX_BASE64_CHUNK % 4, 0);
    let chunks: Vec<_> = encoded_png.as_bytes().chunks(MAX_BASE64_CHUNK).collect();
    let last = chunks.len().saturating_sub(1);

    chunks
        .into_iter()
        .enumerate()
        .map(|(index, payload)| {
            let payload = std::str::from_utf8(payload).expect("base64 is ASCII");
            let more = u8::from(index != last);
            if index == 0 {
                encode_command(format!(
                    "r;id={bitmap_id};fmt=png;source=payload;more={more};{payload}"
                ))
            } else {
                encode_command(format!("r;id={bitmap_id};more={more};{payload}"))
            }
        })
        .collect()
}

fn encode_placement(bitmap_id: u32, placement_id: u32, destination: Destination) -> Vec<u8> {
    // row/col are visible coordinates now; Ratty attaches the resulting
    // placement to this alternate-screen content for later scroll/reflow.
    encode_command(format!(
        "p;id={bitmap_id};pid={placement_id};row={};col={};w={};h={};fit=contain;filter=linear;opacity=1",
        destination.row, destination.col, destination.columns, destination.rows
    ))
}

fn encode_update(placement_id: u32, view: ViewState) -> Vec<u8> {
    encode_command(format!(
        "u;pid={placement_id};src_x={};src_y={};src_w={};src_h={};fit={};filter={};opacity={:.3}",
        view.x,
        view.y,
        view.width,
        view.height,
        view.fit.protocol_value(),
        view.filter.protocol_value(),
        view.opacity
    ))
}

fn encode_deletion(bitmap_id: u32, placement_id: u32) -> [Vec<u8>; 2] {
    [
        encode_command(format!("d;pid={placement_id}")),
        encode_command(format!("d;id={bitmap_id}")),
    ]
}

fn encode_command(body: String) -> Vec<u8> {
    format!("{APC_PREFIX}{body}{APC_END}").into_bytes()
}

trait TerminalBackend {
    fn enable_raw(&mut self) -> io::Result<()>;
    fn enter_alternate(&mut self) -> io::Result<()>;
    fn hide_cursor(&mut self) -> io::Result<()>;
    fn prepare_screen(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn leave_alternate(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;
}

#[derive(Default)]
struct TerminalSetupState {
    raw_enabled: bool,
    alternate_entered: bool,
    cursor_hidden: bool,
}

fn setup_terminal(backend: &mut impl TerminalBackend) -> io::Result<TerminalSetupState> {
    let mut state = TerminalSetupState {
        raw_enabled: true,
        ..TerminalSetupState::default()
    };
    if let Err(error) = backend.enable_raw() {
        restore_terminal(backend, &mut state);
        return Err(error);
    }

    state.alternate_entered = true;
    if let Err(error) = backend.enter_alternate() {
        restore_terminal(backend, &mut state);
        return Err(error);
    }

    state.cursor_hidden = true;
    if let Err(error) = backend.hide_cursor() {
        restore_terminal(backend, &mut state);
        return Err(error);
    }

    if let Err(error) = backend.prepare_screen() {
        restore_terminal(backend, &mut state);
        return Err(error);
    }

    Ok(state)
}

fn restore_terminal(backend: &mut impl TerminalBackend, state: &mut TerminalSetupState) {
    if std::mem::take(&mut state.cursor_hidden) {
        let _ = backend.show_cursor();
    }
    if std::mem::take(&mut state.alternate_entered) {
        let _ = backend.leave_alternate();
    }
    if std::mem::take(&mut state.raw_enabled) {
        let _ = backend.disable_raw();
    }
}

struct CrosstermBackend {
    stdout: Stdout,
}

impl TerminalBackend for CrosstermBackend {
    fn enable_raw(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn enter_alternate(&mut self) -> io::Result<()> {
        execute!(self.stdout, EnterAlternateScreen)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        execute!(self.stdout, Hide)
    }

    fn prepare_screen(&mut self) -> io::Result<()> {
        execute!(self.stdout, Clear(ClearType::All), MoveTo(0, 0))
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(self.stdout, Show)
    }

    fn leave_alternate(&mut self) -> io::Result<()> {
        execute!(self.stdout, LeaveAlternateScreen)
    }

    fn disable_raw(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
}

struct TerminalSession {
    backend: CrosstermBackend,
    setup: TerminalSetupState,
    bitmap_id: u32,
    placement_id: u32,
}

impl TerminalSession {
    fn enter(bitmap_id: u32, placement_id: u32) -> io::Result<Self> {
        let mut backend = CrosstermBackend {
            stdout: io::stdout(),
        };
        let setup = setup_terminal(&mut backend)?;
        Ok(Self {
            backend,
            setup,
            bitmap_id,
            placement_id,
        })
    }

    fn write_commands<I>(&mut self, commands: I) -> io::Result<()>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        for command in commands {
            self.backend.stdout.write_all(&command)?;
        }
        self.backend.stdout.flush()
    }

    fn draw_help(&mut self, view: ViewState) -> io::Result<()> {
        queue!(
            self.backend.stdout,
            MoveTo(0, 0),
            Clear(ClearType::CurrentLine),
            Print("arrows pan | +/- zoom | f fit | n/l filter | [/] opacity | q quit"),
            MoveTo(0, 1),
            Clear(ClearType::CurrentLine),
            Print(format!(
                "crop {}x{}+{},{} | fit {} | filter {} | opacity {:.1}",
                view.width,
                view.height,
                view.x,
                view.y,
                view.fit.protocol_value(),
                view.filter.protocol_value(),
                view.opacity
            ))
        )?;
        self.backend.stdout.flush()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        for command in encode_deletion(self.bitmap_id, self.placement_id) {
            let _ = self.backend.stdout.write_all(&command);
        }
        let _ = self.backend.stdout.flush();
        restore_terminal(&mut self.backend, &mut self.setup);
    }
}

fn main() -> Result<()> {
    let path = env::args_os()
        .nth(1)
        .context("usage: cargo run --example bitmap_pan_zoom -- <image.png>")?;
    let png = fs::read(&path)
        .with_context(|| format!("failed to read PNG from {}", path.to_string_lossy()))?;
    let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .context("input is not a valid PNG")?;
    let (bitmap_width, bitmap_height) = image.dimensions();
    drop(image);
    let encoded_png = base64::engine::general_purpose::STANDARD.encode(&png);

    let mut terminal = TerminalSession::enter(BITMAP_ID, PLACEMENT_ID)
        .context("failed to enter interactive terminal mode")?;
    let mut view = ViewState::new(bitmap_width, bitmap_height);
    terminal.draw_help(view)?;
    terminal.write_commands(encode_registration(BITMAP_ID, &encoded_png))?;
    let (columns, rows) = terminal::size().context("failed to read terminal size")?;
    terminal.write_commands([encode_placement(
        BITMAP_ID,
        PLACEMENT_ID,
        Destination::new(
            2,
            1,
            u32::from(columns.saturating_sub(2).max(1)),
            u32::from(rows.saturating_sub(3).max(1)),
        ),
    )])?;

    loop {
        let Event::Key(key) = event::read().context("failed to read terminal input")? else {
            continue;
        };
        if !key.is_press() {
            continue;
        }

        let pan_x = i64::from((view.width / 20).max(1));
        let pan_y = i64::from((view.height / 20).max(1));
        let changed = match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Left => {
                view.pan(-pan_x, 0);
                true
            }
            KeyCode::Right => {
                view.pan(pan_x, 0);
                true
            }
            KeyCode::Up => {
                view.pan(0, -pan_y);
                true
            }
            KeyCode::Down => {
                view.pan(0, pan_y);
                true
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                view.zoom(Zoom::In);
                true
            }
            KeyCode::Char('-') => {
                view.zoom(Zoom::Out);
                true
            }
            KeyCode::Char('f') => {
                view.cycle_fit();
                true
            }
            KeyCode::Char('n') => {
                view.filter = Filter::Nearest;
                true
            }
            KeyCode::Char('l') => {
                view.filter = Filter::Linear;
                true
            }
            KeyCode::Char('[') => {
                view.opacity = (view.opacity - 0.1).max(0.0);
                true
            }
            KeyCode::Char(']') => {
                view.opacity = (view.opacity + 0.1).min(1.0);
                true
            }
            _ => false,
        };

        if changed {
            terminal.write_commands([encode_update(PLACEMENT_ID, view)])?;
            terminal.draw_help(view)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const APC_END: &str = "\u{1b}\\";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TerminalAction {
        EnableRaw,
        EnterAlternate,
        HideCursor,
        PrepareScreen,
        ShowCursor,
        LeaveAlternate,
        DisableRaw,
    }

    struct MockTerminalBackend {
        fail_at: TerminalAction,
        actions: Vec<TerminalAction>,
    }

    impl MockTerminalBackend {
        fn new(fail_at: TerminalAction) -> Self {
            Self {
                fail_at,
                actions: Vec::new(),
            }
        }

        fn record(&mut self, action: TerminalAction) -> io::Result<()> {
            self.actions.push(action);
            if action == self.fail_at {
                Err(io::Error::other("injected terminal failure"))
            } else {
                Ok(())
            }
        }
    }

    impl TerminalBackend for MockTerminalBackend {
        fn enable_raw(&mut self) -> io::Result<()> {
            self.record(TerminalAction::EnableRaw)
        }

        fn enter_alternate(&mut self) -> io::Result<()> {
            self.record(TerminalAction::EnterAlternate)
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.record(TerminalAction::HideCursor)
        }

        fn prepare_screen(&mut self) -> io::Result<()> {
            self.record(TerminalAction::PrepareScreen)
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.record(TerminalAction::ShowCursor)
        }

        fn leave_alternate(&mut self) -> io::Result<()> {
            self.record(TerminalAction::LeaveAlternate)
        }

        fn disable_raw(&mut self) -> io::Result<()> {
            self.record(TerminalAction::DisableRaw)
        }
    }

    #[test]
    fn enter_alternate_mutation_followed_by_error_still_attempts_reverse_cleanup() {
        let mut backend = MockTerminalBackend::new(TerminalAction::EnterAlternate);

        assert!(setup_terminal(&mut backend).is_err());

        assert_eq!(
            backend.actions,
            vec![
                TerminalAction::EnableRaw,
                TerminalAction::EnterAlternate,
                TerminalAction::LeaveAlternate,
                TerminalAction::DisableRaw,
            ]
        );
    }

    #[test]
    fn hide_cursor_mutation_followed_by_error_still_attempts_reverse_cleanup() {
        let mut backend = MockTerminalBackend::new(TerminalAction::HideCursor);

        assert!(setup_terminal(&mut backend).is_err());

        assert_eq!(
            backend.actions,
            vec![
                TerminalAction::EnableRaw,
                TerminalAction::EnterAlternate,
                TerminalAction::HideCursor,
                TerminalAction::ShowCursor,
                TerminalAction::LeaveAlternate,
                TerminalAction::DisableRaw,
            ]
        );
    }

    #[test]
    fn registration_chunks_base64_on_aligned_4096_character_boundaries() {
        let encoded = "A".repeat(4096 * 2 + 8);

        let chunks = encode_registration(BITMAP_ID, &encoded);

        assert_eq!(chunks.len(), 3);
        for (index, chunk) in chunks.iter().enumerate() {
            let command =
                std::str::from_utf8(chunk).expect("valid example test input should succeed");
            let payload = command
                .strip_suffix(APC_END)
                .expect("valid example test input should succeed")
                .rsplit_once(';')
                .expect("valid example test input should succeed")
                .1;
            assert!(payload.len() <= 4096);
            assert_eq!(payload.len() % 4, 0);
            assert_eq!(command.contains("fmt=png;source=payload"), index == 0);
            assert_eq!(command.contains("more=0"), index == 2);
        }
    }

    #[test]
    fn lifecycle_registers_and_places_once_then_only_updates_before_deletion() {
        let mut commands = encode_registration(BITMAP_ID, "QUJDRA==");
        commands.push(encode_placement(
            BITMAP_ID,
            PLACEMENT_ID,
            Destination::new(2, 1, 80, 24),
        ));
        let mut view = ViewState::new(640, 480);
        view.zoom(Zoom::In);
        commands.push(encode_update(PLACEMENT_ID, view));
        view.pan(16, 8);
        commands.push(encode_update(PLACEMENT_ID, view));
        view.cycle_fit();
        commands.push(encode_update(PLACEMENT_ID, view));
        view.filter = Filter::Nearest;
        commands.push(encode_update(PLACEMENT_ID, view));
        view.opacity = 0.5;
        commands.push(encode_update(PLACEMENT_ID, view));
        commands.extend(encode_deletion(BITMAP_ID, PLACEMENT_ID));

        let commands: Vec<_> = commands
            .iter()
            .map(|command| {
                std::str::from_utf8(command).expect("valid example test input should succeed")
            })
            .collect();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.contains(";r;id=") && command.contains("fmt=png"))
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.contains(";p;"))
                .count(),
            1
        );
        assert!(
            commands[2..commands.len() - 2]
                .iter()
                .all(|command| command.contains(";u;pid="))
        );
        assert!(commands[2].contains("src_w=320;src_h=240"));
        assert!(commands[3].contains("src_x=176;src_y=128"));
        assert!(commands[4].contains("fit=cover"));
        assert!(commands[5].contains("filter=nearest"));
        assert!(commands[6].contains("opacity=0.500"));
        assert!(commands[7].contains(";d;pid=7"));
        assert!(commands[8].contains(";d;id=42"));
    }
}
