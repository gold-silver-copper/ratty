//! CPU PTY-output parsing and Ratatui composition benchmark (no GPU timing).
//!
//! Run with `CARGO_PROFILE_DEV_OPT_LEVEL=3 CARGO_PROFILE_DEV_DEBUG=0 cargo run
//! --example composition_bench`. Five independent samples use identical output
//! traces; the median is printed. Runtime/PTY startup and history seeding are excluded.

use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use ratty::config::AppConfig;
use ratty::inline::TerminalInlineObjects;
use ratty::mouse::TerminalSelection;
use ratty::runtime::{RuntimeOptions, TerminalRuntime};
use ratty::terminal::{TerminalCompositionCache, TerminalWidget};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    ratty::runtime::run_pty_launcher();
    if std::env::args().any(|arg| arg == "--bench-child") {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    let cached = std::env::args().any(|arg| arg == "--cached");
    let mode = if cached { "cached" } else { "full" };
    let frames = 500;
    println!("case,mode,frames,median_ms,composed_rows");
    for case in [
        "typing",
        "output",
        "scrollback",
        "unicode",
        "inline_graphics",
    ] {
        let mut samples = Vec::new();
        for _ in 0..5 {
            samples.push(run(case, frames, cached)?);
        }
        samples.sort_by(|a, b| a.0.total_cmp(&b.0));
        println!(
            "{case},{mode},{frames},{:.3},{}",
            samples[2].0, samples[2].1
        );
    }
    Ok(())
}

fn run(case: &str, frames: usize, cached: bool) -> anyhow::Result<(f64, usize)> {
    let mut config = AppConfig::default();
    config.terminal.default_rows = 40;
    config.terminal.default_cols = 120;
    let command = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "--bench-child".into(),
    ];
    let mut runtime = TerminalRuntime::spawn(
        &config,
        &RuntimeOptions {
            command: Some(command),
            working_dir: None,
        },
    )?;
    let mut graphics = TerminalInlineObjects::default();
    let mut cameras = Vec::new();
    let mut processed = false;
    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 40));
    let selection = TerminalSelection::default();
    let mut cache = TerminalCompositionCache::default();
    let seed = "row with ANSI \x1b[31mred\x1b[0m text and columns 0123456789\r\n".repeat(1000);
    graphics.consume_pty_output(seed.as_bytes(), &mut runtime, &mut cameras, &mut processed);
    let bulk = b"a complete output line with data 0123456789 and more columns\r\n".repeat(24);
    let begin = Instant::now();
    for frame in 0..frames {
        let bytes: &[u8] = match case {
            "typing" => b"a",
            "output" => &bulk,
            "scrollback" => b"",
            "unicode" => "\r中文 界 e\u{301} 👩🏽‍💻 emoji text".as_bytes(),
            "inline_graphics" => b"\x1b_Ga=T,f=32,s=1,v=1,i=7,c=2,r=2;/wAA/w==\x1b\\",
            _ => unreachable!("fixed benchmark cases"),
        };
        graphics.consume_pty_output(bytes, &mut runtime, &mut cameras, &mut processed);
        if case == "scrollback" {
            runtime.set_scrollback(40 + frame % 100);
        }
        buffer.reset();
        let widget = TerminalWidget {
            screen: runtime.screen(),
            selection: &selection,
            theme: &config.theme,
            font_style: config.font.style,
        };
        if cached {
            cache.render(widget, buffer.area, &mut buffer);
        } else {
            widget.render(buffer.area, &mut buffer);
        }
        black_box(&buffer);
        cameras.clear();
    }
    let elapsed = begin.elapsed().as_secs_f64() * 1000.0;
    runtime.shutdown();
    Ok((
        elapsed,
        if cached {
            cache.composed_rows() as usize
        } else {
            frames * 40
        },
    ))
}
