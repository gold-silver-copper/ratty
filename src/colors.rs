//! Host theme colors kept separate from the application's OSC overrides.

use crate::config::ThemeConfig;
use fux_vt::{Rgb, Screen};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostColors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub palette: [Rgb; 16],
}
impl Default for HostColors {
    fn default() -> Self {
        Self::from(&ThemeConfig::default())
    }
}
impl From<&ThemeConfig> for HostColors {
    fn from(theme: &ThemeConfig) -> Self {
        Self {
            foreground: theme.foreground.into(),
            background: theme.background.into(),
            cursor: theme.cursor.into(),
            palette: theme.palette().map(Rgb::from),
        }
    }
}
impl HostColors {
    pub(crate) fn answer(&self, number: u8, bel: bool) -> Option<Vec<u8>> {
        let colors = fux_vt::keys::colour::Colours {
            foreground: Some(self.foreground),
            background: Some(self.background),
            scheme: None,
        };
        if number != 12 {
            return colors.answer(number, bel);
        }
        let Rgb { r, g, b } = self.cursor;
        Some(
            format!(
                "\x1b]12;rgb:{:04x}/{:04x}/{:04x}{}",
                u16::from(r) * 257,
                u16::from(g) * 257,
                u16::from(b) * 257,
                if bel { "\x07" } else { "\x1b\\" }
            )
            .into_bytes(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedColors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub palette: [Rgb; 256],
    pub selection_foreground: Option<Rgb>,
    pub selection_background: Option<Rgb>,
}
impl ResolvedColors {
    pub(crate) fn new(screen: &Screen, theme: &ThemeConfig) -> Self {
        let host = HostColors::from(theme);
        Self {
            foreground: screen.dynamic_color(10).unwrap_or(host.foreground),
            background: screen.dynamic_color(11).unwrap_or(host.background),
            cursor: screen.dynamic_color(12).unwrap_or(host.cursor),
            palette: std::array::from_fn(|i| {
                screen
                    .palette_color(i as u8)
                    .unwrap_or_else(|| indexed(i as u8, &host.palette))
            }),
            selection_foreground: screen.dynamic_color(19),
            selection_background: screen.dynamic_color(17),
        }
    }
}

// fux-vt exposes application overrides, but not its xterm fallback palette.
fn indexed(index: u8, host: &[Rgb; 16]) -> Rgb {
    match index {
        0..=15 => host[usize::from(index)],
        16..=231 => {
            let index = index - 16;
            let component = |v| if v == 0 { 0 } else { 55 + v * 40 };
            [
                component(index / 36),
                component(index % 36 / 6),
                component(index % 6),
            ]
            .into()
        }
        232..=255 => {
            let shade = 8 + (index - 232) * 10;
            [shade; 3].into()
        }
    }
}

pub(crate) fn channels(Rgb { r, g, b }: Rgb) -> [u8; 3] {
    [r, g, b]
}
