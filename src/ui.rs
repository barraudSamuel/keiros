use std::io::{self, IsTerminal};

use crate::store::{EventType, StorageKind};

#[derive(Clone, Copy)]
enum Tone {
    Accent,
    Muted,
    Success,
    Warning,
    Danger,
    Info,
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

pub struct TableCell {
    raw: String,
    align: Align,
    tone: Option<Tone>,
}

impl TableCell {
    pub fn plain(value: impl Into<String>) -> Self {
        Self {
            raw: value.into(),
            align: Align::Left,
            tone: None,
        }
    }

    pub fn right(value: impl Into<String>) -> Self {
        Self {
            raw: value.into(),
            align: Align::Right,
            tone: None,
        }
    }

    pub fn success(mut self) -> Self {
        self.tone = Some(Tone::Success);
        self
    }

    pub fn warning(mut self) -> Self {
        self.tone = Some(Tone::Warning);
        self
    }

    pub fn danger(mut self) -> Self {
        self.tone = Some(Tone::Danger);
        self
    }

    pub fn info(mut self) -> Self {
        self.tone = Some(Tone::Info);
        self
    }

    fn render(&self, ui: &Ui, width: usize) -> String {
        let padded = match self.align {
            Align::Left => format!("{:<width$}", self.raw, width = width),
            Align::Right => format!("{:>width$}", self.raw, width = width),
        };

        match self.tone {
            Some(tone) => ui.paint(&padded, tone),
            None => padded,
        }
    }

    fn width(&self) -> usize {
        self.raw.len()
    }
}

pub struct Ui {
    color: bool,
}

impl Ui {
    pub fn stdout() -> Self {
        Self::new(io::stdout().is_terminal())
    }

    pub fn stderr() -> Self {
        Self::new(io::stderr().is_terminal())
    }

    fn new(is_terminal: bool) -> Self {
        let color = is_terminal
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM")
                .map(|term| term != "dumb")
                .unwrap_or(true);
        Self { color }
    }

    pub fn title(&self, title: &str, context: impl AsRef<str>) -> String {
        let context = context.as_ref();
        let heading = if context.is_empty() {
            title.to_string()
        } else {
            format!("{title}: {context}")
        };
        let rule = "-".repeat(heading.len().max(12));
        format!(
            "{}\n{}",
            self.paint(&heading, Tone::Accent),
            self.paint(&rule, Tone::Muted)
        )
    }

    pub fn key_values(&self, rows: &[(&str, String)]) -> String {
        let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
        rows.iter()
            .map(|(label, value)| {
                let padded = format!("{:<width$}", format!("{label}:"), width = width + 1);
                format!("{} {}", self.paint(&padded, Tone::Muted), value)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn table(&self, headers: &[&str], rows: &[Vec<TableCell>]) -> String {
        let mut widths = headers
            .iter()
            .map(|header| header.len())
            .collect::<Vec<_>>();
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.width());
            }
        }

        let header_line = headers
            .iter()
            .enumerate()
            .map(|(index, header)| format!("{:<width$}", header, width = widths[index]))
            .collect::<Vec<_>>()
            .join(" | ");
        let separator = widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("-+-");

        let mut lines = Vec::with_capacity(rows.len() + 2);
        lines.push(self.paint(&header_line, Tone::Info));
        lines.push(self.paint(&separator, Tone::Muted));
        lines.extend(rows.iter().map(|row| {
            row.iter()
                .enumerate()
                .map(|(index, cell)| cell.render(self, widths[index]))
                .collect::<Vec<_>>()
                .join(" | ")
        }));
        lines.join("\n")
    }

    pub fn success(&self, message: impl AsRef<str>) -> String {
        self.callout("OK", message.as_ref(), Tone::Success)
    }

    pub fn warning(&self, message: impl AsRef<str>) -> String {
        self.callout("WARN", message.as_ref(), Tone::Warning)
    }

    pub fn error(&self, message: impl AsRef<str>) -> String {
        self.callout("ERR", message.as_ref(), Tone::Danger)
    }

    pub fn muted(&self, message: impl AsRef<str>) -> String {
        self.paint(message.as_ref(), Tone::Muted)
    }

    pub fn badge(&self, label: &str, tone: BadgeTone) -> String {
        let tone = match tone {
            BadgeTone::Accent => Tone::Accent,
            BadgeTone::Success => Tone::Success,
            BadgeTone::Warning => Tone::Warning,
            BadgeTone::Danger => Tone::Danger,
            BadgeTone::Info => Tone::Info,
        };
        self.paint(&format!("[{label}]"), tone)
    }

    pub fn event_cell(&self, event: EventType) -> TableCell {
        match event {
            EventType::Create => TableCell::plain("CREATE").success(),
            EventType::Modify => TableCell::plain("MODIFY").info(),
            EventType::Delete => TableCell::plain("DELETE").danger(),
        }
    }

    pub fn storage_cell(&self, kind: StorageKind) -> TableCell {
        match kind {
            StorageKind::Full => TableCell::plain("FULL").success(),
            StorageKind::Patch => TableCell::plain("PATCH").warning(),
            StorageKind::None => TableCell::plain("NONE").danger(),
        }
    }

    pub fn render_diff(&self, diff: &str) -> String {
        diff.lines()
            .map(|line| {
                if line.starts_with("@@") {
                    self.paint(line, Tone::Info)
                } else if line.starts_with("+++") || line.starts_with("---") {
                    self.paint(line, Tone::Accent)
                } else if line.starts_with('+') {
                    self.paint(line, Tone::Success)
                } else if line.starts_with('-') {
                    self.paint(line, Tone::Danger)
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn callout(&self, label: &str, message: &str, tone: Tone) -> String {
        format!("{} {}", self.paint(&format!("[{label}]"), tone), message)
    }

    fn paint(&self, text: &str, tone: Tone) -> String {
        if !self.color {
            return text.to_string();
        }

        let code = match tone {
            Tone::Accent => "1;36",
            Tone::Muted => "90",
            Tone::Success => "1;32",
            Tone::Warning => "1;33",
            Tone::Danger => "1;31",
            Tone::Info => "1;34",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

pub enum BadgeTone {
    Accent,
    Success,
    Warning,
    Danger,
    Info,
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes as f64;
    let mut unit = 0_usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
