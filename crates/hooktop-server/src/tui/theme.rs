//! Colours for the TUI: the official ayu palettes, dark and light. The
//! background is never painted, so the terminal's own shows through.
//!
//! On terminals without 24-bit colour, each ayu colour is drawn as its
//! nearest match in the standard 256-colour palette, so the look survives.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Light,
}

impl ThemeName {
    pub const ALL: [ThemeName; 2] = [Self::Dark, Self::Light];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
            "dark" | "ayu-dark" => Some(Self::Dark),
            "light" | "ayu-light" => Some(Self::Light),
            _ => None,
        }
    }

    /// `HOOKTOP_THEME` if set; otherwise Ayu Light when the terminal says
    /// its background is light (`COLORFGBG`), else Ayu Dark.
    pub fn detect() -> Self {
        if let Some(t) = std::env::var("HOOKTOP_THEME").ok().as_deref().and_then(Self::parse) {
            return t;
        }
        let light_bg = std::env::var("COLORFGBG")
            .ok()
            .and_then(|v| v.rsplit(';').next().map(str::to_string))
            .is_some_and(|bg| matches!(bg.as_str(), "7" | "15"));
        if light_bg { Self::Light } else { Self::Dark }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Ayu Dark",
            Self::Light => "Ayu Light",
        }
    }

    pub fn theme(self) -> Theme {
        let t = self.palette();
        if truecolor() { t } else { t.to_256() }
    }

    fn palette(self) -> Theme {
        let rgb = |hex: u32| Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
        match self {
            Self::Dark => Theme {
                ink: rgb(0xbfbdb6),
                accent: rgb(0xe6b450),
                prompt: rgb(0xffb454),
                turn: rgb(0xff8f40),
                sub: rgb(0x95e6cb),
                read: rgb(0x59c2ff),
                write: rgb(0xaad94c),
                exec: rgb(0xd2a6ff),
                other: rgb(0x5a6673),
                ok: rgb(0xaad94c),
                err: rgb(0xf07178),
                muted: rgb(0x8a8986),
                faint: rgb(0x5a6378),
                line: rgb(0x333b44),
                select: Some(rgb(0x1f2a38)),
            },
            Self::Light => Theme {
                ink: rgb(0x5c6166),
                accent: rgb(0xf29718),
                prompt: rgb(0xeba400),
                turn: rgb(0xfa8532),
                sub: rgb(0x4cbf99),
                read: rgb(0x22a4e6),
                write: rgb(0x86b300),
                exec: rgb(0xa37acc),
                other: rgb(0xadaeb1),
                ok: rgb(0x86b300),
                err: rgb(0xe65050),
                muted: rgb(0x787b80),
                faint: rgb(0x9aa3ae),
                line: rgb(0xcfd4da),
                select: Some(rgb(0xe1e8f0)),
            },
        }
    }
}

/// Does the terminal take 24-bit colour? (`COLORTERM=truecolor` or
/// `24bit`, which most modern terminals set.)
pub fn truecolor() -> bool {
    static TC: OnceLock<bool> = OnceLock::new();
    *TC.get_or_init(|| {
        let ct = std::env::var("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        let term = std::env::var("TERM").unwrap_or_default().to_ascii_lowercase();
        matches!(ct.as_str(), "truecolor" | "24bit") || term.contains("truecolor") || term.contains("direct")
    })
}

/// The nearest colour in the xterm 256-colour palette (its 6×6×6 cube and
/// 24 greys; the first 16 vary by terminal, so they're skipped).
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let dist = |a: (i32, i32, i32)| {
        let (dr, dg, db) = (a.0 - r as i32, a.1 - g as i32, a.2 - b as i32);
        dr * dr + dg * dg + db * db
    };
    let near = |c: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| (**l - c as i32).abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi = ((avg - 8).max(0) / 10).min(23);
    let grey = 8 + 10 * gi;
    if dist((grey, grey, grey)) < dist(cube) {
        232 + gi as u8
    } else {
        cube_idx as u8
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Body text.
    pub ink: Color,
    pub accent: Color,
    pub prompt: Color,
    pub turn: Color,
    pub sub: Color,
    pub read: Color,
    pub write: Color,
    pub exec: Color,
    pub other: Color,
    pub ok: Color,
    pub err: Color,
    pub muted: Color,
    pub faint: Color,
    pub line: Color,
    /// Background of the selected row; `None` means reverse video.
    pub select: Option<Color>,
}

impl Theme {
    /// The same palette in 256 colours.
    pub fn to_256(self) -> Self {
        let c = |c: Color| match c {
            Color::Rgb(r, g, b) => Color::Indexed(nearest_256(r, g, b)),
            other => other,
        };
        Self {
            ink: c(self.ink),
            accent: c(self.accent),
            prompt: c(self.prompt),
            turn: c(self.turn),
            sub: c(self.sub),
            read: c(self.read),
            write: c(self.write),
            exec: c(self.exec),
            other: c(self.other),
            ok: c(self.ok),
            err: c(self.err),
            muted: c(self.muted),
            faint: c(self.faint),
            line: c(self.line),
            select: self.select.map(c),
        }
    }

    pub fn fg(&self, c: Color) -> Style {
        Style::new().fg(c)
    }

    pub fn muted(&self) -> Style {
        Style::new().fg(self.muted)
    }

    pub fn faint(&self) -> Style {
        Style::new().fg(self.faint)
    }

    pub fn selected(&self) -> Style {
        match self.select {
            Some(bg) => Style::new().bg(bg).add_modifier(Modifier::BOLD),
            None => Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    pub fn border(&self, focused: bool) -> Style {
        if focused {
            Style::new().fg(self.accent)
        } else {
            Style::new().fg(self.line)
        }
    }

    /// Dot colour for a tool, by what it does: read, write, run, delegate.
    pub fn tool(&self, name: &str) -> Color {
        match name {
            "Read" | "Grep" | "Glob" | "LS" | "WebFetch" | "WebSearch" | "NotebookRead" => self.read,
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => self.write,
            "Bash" | "BashOutput" | "KillShell" | "KillBash" => self.exec,
            "Agent" | "Task" => self.sub,
            _ => self.other,
        }
    }
}
