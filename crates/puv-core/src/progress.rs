use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

/// A single-line download meter on stderr. Pipelines get one plain line.
pub struct Progress {
    label: String,
    total: Option<u64>,
    done: u64,
    tty: bool,
    drawn: bool,
    spin: u64,
    last: Instant,
}

impl Progress {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            total: None,
            done: 0,
            tty: io::stderr().is_terminal(),
            drawn: false,
            spin: 0,
            last: Instant::now(),
        }
    }

    pub fn set_total(&mut self, total: u64) {
        self.total = Some(total);
    }

    /// Show the meter before the first byte arrives.
    pub fn begin(&mut self) {
        if !self.tty {
            if !self.drawn {
                eprintln!("download {}", self.label);
                self.drawn = true;
            }
            return;
        }
        self.draw();
    }

    pub fn advance(&mut self, bytes: u64) {
        self.done = self.done.saturating_add(bytes);
        self.spin = self.spin.wrapping_add(1);
        if !self.tty {
            self.begin();
            return;
        }
        if !self.drawn || self.last.elapsed() >= Duration::from_millis(40) {
            self.draw();
        }
    }

    pub fn finish(&mut self) {
        if !self.drawn {
            self.begin();
        }
        if self.tty {
            self.draw();
            eprintln!();
        }
    }

    fn draw(&mut self) {
        let width = 24usize;
        let (bar, amounts) = if let Some(total) = self.total.filter(|total| *total > 0) {
            let filled = ((self.done as f64 / total as f64) * width as f64) as usize;
            let filled = filled.min(width);
            (
                format!(
                    "{}{}",
                    crate::paint(&"█".repeat(filled), "32", true),
                    crate::paint(&"░".repeat(width - filled), "2", true)
                ),
                crate::paint(
                    &format!("{} / {}", human_bytes(self.done), human_bytes(total)),
                    "1",
                    true,
                ),
            )
        } else {
            let marker = 5usize;
            let shift = (self.spin as usize) % width;
            let raw: String = "█".repeat(marker) + &"░".repeat(width - marker);
            let chars: Vec<char> = raw.chars().collect();
            let rotated: String = chars.iter().cycle().skip(shift).take(width).collect();
            (
                crate::paint(&rotated, "36", true),
                crate::paint(&human_bytes(self.done), "1", true),
            )
        };
        let label = crate::paint(&self.label, "1;36", true);
        eprint!(
            "\r{} {label}  {bar}  {amounts}",
            crate::paint("download", "36", true)
        );
        let _ = io::stderr().flush();
        self.drawn = true;
        self.last = Instant::now();
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
