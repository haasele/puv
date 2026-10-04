use std::io::{self, IsTerminal};

pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

pub fn stderr_is_tty() -> bool {
    io::stderr().is_terminal()
}

pub fn paint(text: &str, code: &str, enabled: bool) -> String {
    if enabled && std::env::var_os("NO_COLOR").is_none() {
        format!("\u{1b}[{code}m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}
