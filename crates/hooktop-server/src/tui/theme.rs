//! Colours for the TUI: the three official ayu palettes, plus a
//! `terminal` theme of plain ANSI colours for terminals without 24-bit
//! colour. The background is never painted, so the terminal's own shows.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Mirage,
    Light,
    Terminal,
}

impl ThemeName {
    pub const ALL: [ThemeName; 4] = [Self::Dark, Self::Mirage, Self::Light, Self::Terminal];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
            "dark" | "ayu-dark" => Some(Self::Dark),
            "mirage" | "ayu-mirage" => Some(Self::Mirage),
            "light" | "ayu-light" => Some(Self::Light),
            "terminal" | "ansi" => Some(Self::Terminal),
            _ => None,
        }
    }

    /// Ayu Dark on terminals that announce 24-bit colour, ANSI otherwise.
    pub fn detect() -> Self {
        if let Some(t) = std::env::var("HOOKTOP_THEME").ok().as_deref().and_then(Self::parse) {
            return t;
        }
        match std::env::var("COLORTERM")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "truecolor" | "24bit" => Self::Dark,
            _ => Self::Terminal,
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Ayu Dark",
            Self::Mirage => "Ayu Mirage",
            Self::Light => "Ayu Light",
            Self::Terminal => "Terminal",
        }
    }

    pub fn theme(self) -> Theme {
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
            Self::Mirage => Theme {
                ink: rgb(0xcccac2),
                accent: rgb(0xffcc66),
                prompt: rgb(0xffcd66),
                turn: rgb(0xffa659),
                sub: rgb(0x95e6cb),
                read: rgb(0x73d0ff),
                write: rgb(0xd5ff80),
                exec: rgb(0xdfbfff),
                other: rgb(0x6e7c8f),
                ok: rgb(0xd5ff80),
                err: rgb(0xf28779),
                muted: rgb(0x9a998f),
                faint: rgb(0x707a8c),
                line: rgb(0x4a5568),
                select: Some(rgb(0x33415e)),
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
            Self::Terminal => Theme {
                ink: Color::Reset,
                accent: Color::Yellow,
                prompt: Color::Yellow,
                turn: Color::LightRed,
                sub: Color::Cyan,
                read: Color::Blue,
                write: Color::Green,
                exec: Color::Magenta,
                other: Color::DarkGray,
                ok: Color::Green,
                err: Color::Red,
                muted: Color::Gray,
                faint: Color::DarkGray,
                line: Color::DarkGray,
                select: None,
            },
        }
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
