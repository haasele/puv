use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{paint, stderr_is_tty};

const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const COLORS: &[&str] = &["36", "96", "35", "95", "34", "94"];

/// A one-line status spinner on stderr. Pipelines stay quiet.
pub struct Spinner {
    label: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    started: Instant,
    tty: bool,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub fn start(label: impl Into<String>) -> Self {
        let text = label.into();
        let tty = stderr_is_tty();
        let label = Arc::new(Mutex::new(text));
        let stop = Arc::new(AtomicBool::new(!tty));
        if !tty {
            return Self {
                label,
                stop,
                started: Instant::now(),
                tty: false,
                handle: None,
            };
        }
        let label_thread = Arc::clone(&label);
        let stop_thread = Arc::clone(&stop);
        eprint!("\x1b[?25l");
        let handle = thread::spawn(move || {
            let mut frame = 0usize;
            while !stop_thread.load(Ordering::Relaxed) {
                let caption = label_thread
                    .lock()
                    .map(|value| value.clone())
                    .unwrap_or_default();
                let glyph = paint(
                    FRAMES[frame % FRAMES.len()],
                    COLORS[frame % COLORS.len()],
                    true,
                );
                eprint!("\r\x1b[2K{} {}", glyph, paint(&caption, "1", true));
                let _ = io::stderr().flush();
                frame = frame.wrapping_add(1);
                thread::sleep(Duration::from_millis(70));
            }
        });
        Self {
            label,
            stop,
            started: Instant::now(),
            tty: true,
            handle: Some(handle),
        }
    }

    pub fn set(&self, label: impl Into<String>) {
        if let Ok(mut slot) = self.label.lock() {
            *slot = label.into();
        }
    }

    pub fn finish(mut self) {
        if self.tty {
            let minimum = Duration::from_millis(700);
            let elapsed = self.started.elapsed();
            if elapsed < minimum {
                thread::sleep(minimum - elapsed);
            }
        }
        self.stop_and_clear();
    }

    fn stop_and_clear(&mut self) {
        if self.stop.swap(true, Ordering::Relaxed) && self.handle.is_none() {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if self.tty {
            eprint!("\r\x1b[2K\x1b[?25h");
            let _ = io::stderr().flush();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop_and_clear();
    }
}
