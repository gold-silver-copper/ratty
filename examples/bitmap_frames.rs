use std::{
    io::{self, Stdout, Write},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use clap::Parser;
use image::ImageEncoder as _;
use ratatui::crossterm::{
    cursor::{Hide, MoveTo, Show},
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

#[derive(Parser)]
#[command(about = "Stream generated RGBA8 frames through Ratty's bitmap surface protocol")]
struct Args {
    /// Frames per second.
    #[arg(long, default_value_t = 15)]
    fps: u32,

    /// Run duration in seconds.
    #[arg(long, default_value_t = 10.0)]
    duration: f64,

    /// Bitmap width in pixels.
    #[arg(long, default_value_t = 320)]
    width: u32,

    /// Bitmap height in pixels.
    #[arg(long, default_value_t = 180)]
    height: u32,
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

fn encode_placement(
    bitmap_id: u32,
    placement_id: u32,
    row: u16,
    col: u16,
    columns: u32,
    rows: u32,
) -> Vec<u8> {
    // row/col are visible coordinates now; Ratty attaches the resulting
    // placement to this alternate-screen content for later scroll/reflow.
    encode_command(format!(
        "p;id={bitmap_id};pid={placement_id};row={row};col={col};w={columns};h={rows};fit=contain;filter=linear;opacity=1"
    ))
}

fn encode_frame(
    bitmap_id: u32,
    sequence: u32,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Vec<Vec<u8>>> {
    ensure!(width > 0 && height > 0, "frame dimensions must be nonzero");
    let expected_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .context("frame dimensions overflow")?;
    ensure!(
        rgba.len() == expected_len,
        "RGBA8 frame length does not match its dimensions"
    );

    let encoded = base64::engine::general_purpose::STANDARD.encode(rgba);
    debug_assert_eq!(MAX_BASE64_CHUNK % 4, 0);
    let chunks: Vec<_> = encoded.as_bytes().chunks(MAX_BASE64_CHUNK).collect();
    let last = chunks.len().saturating_sub(1);

    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(index, payload)| {
            let payload = std::str::from_utf8(payload).expect("base64 is ASCII");
            let more = u8::from(index != last);
            if index == 0 {
                encode_command(format!(
                    "f;id={bitmap_id};seq={sequence};fmt=rgba8;w={width};h={height};more={more};{payload}"
                ))
            } else {
                encode_command(format!(
                    "f;id={bitmap_id};seq={sequence};more={more};{payload}"
                ))
            }
        })
        .collect())
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

struct LatestFrameScheduler {
    interval: Duration,
    next_sequence: Option<u32>,
}

impl LatestFrameScheduler {
    const fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_sequence: Some(1),
        }
    }

    fn due_sequence(&mut self, elapsed: Duration) -> Option<u32> {
        let next_sequence = self.next_sequence?;
        let latest_tick = elapsed.as_nanos() / self.interval.as_nanos();
        let latest_tick = u32::try_from(latest_tick).unwrap_or(u32::MAX);
        if latest_tick < next_sequence {
            return None;
        }

        self.next_sequence = latest_tick.checked_add(1);
        Some(latest_tick)
    }

    fn next_deadline(&self) -> Duration {
        self.next_sequence.map_or(Duration::MAX, |sequence| {
            self.interval.saturating_mul(sequence)
        })
    }
}

#[derive(Debug)]
struct TimingConfig {
    interval: Duration,
    run_duration: Duration,
}

fn validate_timing(fps: u32, duration_seconds: f64) -> Result<TimingConfig> {
    ensure!(fps > 0, "--fps must be greater than zero");
    ensure!(
        duration_seconds.is_finite() && duration_seconds > 0.0,
        "--duration must be a finite positive number"
    );

    let interval = Duration::try_from_secs_f64(1.0 / f64::from(fps))
        .context("--fps cannot be represented as a frame interval")?;
    ensure!(!interval.is_zero(), "--fps is too large");
    let run_duration = Duration::try_from_secs_f64(duration_seconds)
        .context("--duration is too large to represent")?;

    // The loop emits only while elapsed < run_duration. At nanosecond
    // resolution, this is the largest tick that can become due.
    let maximum_due_tick = run_duration.as_nanos().saturating_sub(1) / interval.as_nanos();
    ensure!(
        maximum_due_tick <= u128::from(u32::MAX),
        "--fps and --duration can exceed the u32 frame sequence capacity"
    );

    Ok(TimingConfig {
        interval,
        run_duration,
    })
}

fn generate_frame(width: u32, height: u32, sequence: u32) -> Result<Vec<u8>> {
    let len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .context("frame dimensions overflow")?;
    let mut rgba = vec![0; len];
    let square_size = (width.min(height) / 5).max(1);
    let travel = width.saturating_sub(square_size).saturating_add(1);
    let square_x = sequence.wrapping_mul(6) % travel;
    let square_y = height.saturating_sub(square_size) / 2;

    for y in 0..height {
        for x in 0..width {
            let offset = usize::try_from((y * width + x) * 4).expect("validated frame fits usize");
            rgba[offset] = ((u64::from(x) * 255) / u64::from(width)) as u8;
            rgba[offset + 1] = ((u64::from(y) * 255) / u64::from(height)) as u8;
            rgba[offset + 2] = sequence.wrapping_mul(3) as u8;
            rgba[offset + 3] = 255;

            if x >= square_x
                && x < square_x + square_size
                && y >= square_y
                && y < square_y + square_size
            {
                rgba[offset..offset + 4].copy_from_slice(&[255, 240, 32, 255]);
            }
        }
    }

    Ok(rgba)
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .context("failed to encode initial PNG")?;
    Ok(png)
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

    fn draw_status(&mut self, sequence: u32, fps: u32) -> io::Result<()> {
        queue!(
            self.backend.stdout,
            MoveTo(0, 0),
            Clear(ClearType::CurrentLine),
            Print(format!(
                "Ratty RGBA8 bitmap stream | target {fps} FPS | sequence {sequence}"
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
    let args = Args::parse();
    ensure!(
        args.width > 0 && args.height > 0,
        "--width and --height must be greater than zero"
    );

    let timing = validate_timing(args.fps, args.duration)?;
    let initial_rgba = generate_frame(args.width, args.height, 0)?;
    let png = encode_png(args.width, args.height, &initial_rgba)?;
    let encoded_png = base64::engine::general_purpose::STANDARD.encode(png);

    let mut terminal =
        TerminalSession::enter(BITMAP_ID, PLACEMENT_ID).context("failed to enter terminal mode")?;
    terminal.write_commands(encode_registration(BITMAP_ID, &encoded_png))?;
    let (columns, rows) = terminal::size().context("failed to read terminal size")?;
    terminal.write_commands([encode_placement(
        BITMAP_ID,
        PLACEMENT_ID,
        1,
        0,
        u32::from(columns.max(1)),
        u32::from(rows.saturating_sub(1).max(1)),
    )])?;
    terminal.draw_status(0, args.fps)?;

    let started = Instant::now();
    let mut scheduler = LatestFrameScheduler::new(timing.interval);
    loop {
        let elapsed = started.elapsed();
        if elapsed >= timing.run_duration {
            break;
        }

        if let Some(sequence) = scheduler.due_sequence(elapsed) {
            let rgba = generate_frame(args.width, args.height, sequence)?;
            terminal.write_commands(encode_frame(
                BITMAP_ID,
                sequence,
                args.width,
                args.height,
                &rgba,
            )?)?;
            terminal.draw_status(sequence, args.fps)?;
            continue;
        }

        let wait = scheduler
            .next_deadline()
            .saturating_sub(elapsed)
            .min(timing.run_duration.saturating_sub(elapsed));
        thread::sleep(wait);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const APC_END: &str = "\u{1b}\\";

    fn command_text(commands: &[Vec<u8>]) -> Vec<&str> {
        commands
            .iter()
            .map(|command| {
                std::str::from_utf8(command).expect("valid example test input should succeed")
            })
            .collect()
    }

    fn payload(command: &str) -> &str {
        command
            .strip_suffix(APC_END)
            .expect("valid example test input should succeed")
            .rsplit_once(';')
            .expect("valid example test input should succeed")
            .1
    }

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
    fn partial_terminal_setup_attempts_reverse_cleanup() {
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
    fn lifecycle_registers_and_places_once_then_sends_monotonic_rgba_frames_and_deletes() {
        let mut commands = encode_registration(BITMAP_ID, &"A".repeat(4100));
        commands.push(encode_placement(BITMAP_ID, PLACEMENT_ID, 2, 1, 80, 24));
        commands.extend(
            encode_frame(BITMAP_ID, 1, 2, 2, &[0; 16])
                .expect("valid example test input should succeed"),
        );
        commands.extend(
            encode_frame(BITMAP_ID, 3, 2, 2, &[1; 16])
                .expect("valid example test input should succeed"),
        );
        commands.extend(encode_deletion(BITMAP_ID, PLACEMENT_ID));

        let commands = command_text(&commands);
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
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.contains(";f;") && command.contains("fmt=rgba8"))
                .map(|command| *command)
                .collect::<Vec<_>>(),
            vec![
                "\u{1b}_ratty;i;f;id=42;seq=1;fmt=rgba8;w=2;h=2;more=0;AAAAAAAAAAAAAAAAAAAAAA==\u{1b}\\",
                "\u{1b}_ratty;i;f;id=42;seq=3;fmt=rgba8;w=2;h=2;more=0;AQEBAQEBAQEBAQEBAQEBAQ==\u{1b}\\",
            ]
        );
        assert!(commands[commands.len() - 2].contains(";d;pid=7"));
        assert!(commands[commands.len() - 1].contains(";d;id=42"));
    }

    #[test]
    fn frame_chunks_base64_on_aligned_4096_character_boundaries() {
        let rgba = vec![7; 3_075];

        let chunks = encode_frame(BITMAP_ID, 9, 1, 3_075 / 4, &rgba)
            .expect_err("invalid example test input should be rejected");
        assert!(chunks.to_string().contains("length"));

        let rgba = vec![7; 4_096 * 3 / 4 + 4];
        let chunks = encode_frame(BITMAP_ID, 9, 1, rgba.len() as u32 / 4, &rgba)
            .expect("valid example test input should succeed");
        assert_eq!(chunks.len(), 2);
        for (index, command) in command_text(&chunks).iter().enumerate() {
            assert!(payload(command).len() <= 4096);
            assert_eq!(payload(command).len() % 4, 0);
            assert_eq!(command.contains("fmt=rgba8;w=1;h=769"), index == 0);
            assert_eq!(command.contains("more=0"), index == 1);
            if index == 1 {
                assert!(command.starts_with("\u{1b}_ratty;i;f;id=42;seq=9;more=0;"));
            }
        }
    }

    #[test]
    fn scheduler_skips_obsolete_ticks_instead_of_bursting() {
        let mut scheduler = LatestFrameScheduler::new(std::time::Duration::from_millis(100));

        assert_eq!(
            scheduler.due_sequence(std::time::Duration::from_millis(99)),
            None
        );
        assert_eq!(
            scheduler.due_sequence(std::time::Duration::from_millis(100)),
            Some(1)
        );
        assert_eq!(
            scheduler.due_sequence(std::time::Duration::from_millis(450)),
            Some(4)
        );
        assert_eq!(
            scheduler.due_sequence(std::time::Duration::from_millis(451)),
            None
        );
        assert_eq!(
            scheduler.next_deadline(),
            std::time::Duration::from_millis(500)
        );
    }

    #[test]
    fn timing_validation_rejects_duration_conversion_overflow() {
        let error =
            validate_timing(15, 1e300).expect_err("invalid example test input should be rejected");

        assert!(error.to_string().contains("--duration"));
    }

    #[test]
    fn timing_validation_rejects_sequence_capacity_overflow_and_accepts_boundary() {
        let overflow = f64::from(u32::MAX) + 2.0;
        let boundary = f64::from(u32::MAX) + 1.0;

        assert!(validate_timing(1, overflow).is_err());
        assert!(validate_timing(1, boundary).is_ok());
    }

    #[test]
    fn scheduler_emits_u32_max_at_most_once() {
        let mut scheduler = LatestFrameScheduler::new(std::time::Duration::from_nanos(1));
        let saturated = std::time::Duration::from_nanos(u64::from(u32::MAX));

        assert_eq!(scheduler.due_sequence(saturated), Some(u32::MAX));
        assert_eq!(
            scheduler.due_sequence(saturated.saturating_add(std::time::Duration::from_secs(1))),
            None
        );
        assert_eq!(scheduler.next_deadline(), std::time::Duration::MAX);
    }
}
