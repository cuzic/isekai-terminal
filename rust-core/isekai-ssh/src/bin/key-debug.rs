//! `isekai-key-debug` — reads stdin in raw mode and prints every byte
//! sequence as hex + ASCII, so you can verify what escape sequences the
//! terminal is actually sending through isekai-ssh.
//!
//! Usage (on the server):
//!   isekai-key-debug
//!
//! Press Ctrl-C to exit. Each input chunk is printed as:
//!   HEX: 1b 5b 41    TXT: . [ A   (ArrowUp)
//!
//! Known VT sequences are annotated with their name in parentheses.

use std::io::Read;

fn main() {
    // Enable raw mode on the local terminal so we see every byte.
    let _guard = RawMode::enable();

    let mut buf = [0u8; 256];
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();

    eprintln!("isekai-key-debug: reading stdin (Ctrl-C to exit)\n");

    loop {
        match handle.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let bytes = &buf[..n];
                print_chunk(bytes);
                std::io::Write::flush(&mut std::io::stdout()).ok();
            }
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }
    }
}

fn print_chunk(bytes: &[u8]) {
    // Hex part
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    print!("HEX: {:<48}", hex.join(" "));

    // ASCII part (printable otherwise '.')
    let txt: String = bytes.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }).collect();
    print!(" TXT: {:<32}", txt);

    // Annotation
    if let Some(name) = annotate(bytes) {
        print!(" ({name})");
    }
    println!();
}

fn annotate(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        // Arrow keys
        b"\x1b[A" => Some("ArrowUp"),
        b"\x1b[B" => Some("ArrowDown"),
        b"\x1b[C" => Some("ArrowRight"),
        b"\x1b[D" => Some("ArrowLeft"),
        // SS3 arrow keys (DECCKM)
        b"\x1bOA" => Some("ArrowUp(SS3)"),
        b"\x1bOB" => Some("ArrowDown(SS3)"),
        b"\x1bOC" => Some("ArrowRight(SS3)"),
        b"\x1bOD" => Some("ArrowLeft(SS3)"),
        // Home/End
        b"\x1b[H" => Some("Home"),
        b"\x1b[F" => Some("End"),
        b"\x1bOH" => Some("Home(SS3)"),
        b"\x1bOF" => Some("End(SS3)"),
        // PageUp/PageDown
        b"\x1b[5~" => Some("PageUp"),
        b"\x1b[6~" => Some("PageDown"),
        // Delete
        b"\x1b[3~" => Some("Delete"),
        // Function keys
        b"\x1bOP" => Some("F1"),
        b"\x1bOQ" => Some("F2"),
        b"\x1bOR" => Some("F3"),
        b"\x1bOS" => Some("F4"),
        b"\x1b[15~" => Some("F5"),
        b"\x1b[17~" => Some("F6"),
        b"\x1b[18~" => Some("F7"),
        b"\x1b[19~" => Some("F8"),
        b"\x1b[20~" => Some("F9"),
        b"\x1b[21~" => Some("F10"),
        b"\x1b[23~" => Some("F11"),
        b"\x1b[24~" => Some("F12"),
        // Tab variants
        b"\x1b[Z" => Some("Shift+Tab"),
        b"\x09" => Some("Tab"),
        // Special
        b"\x0d" => Some("Enter"),
        b"\x0a" => Some("Ctrl+J"),
        b"\x1b" => Some("Escape"),
        b"\x7f" => Some("Backspace"),
        b"\x03" => Some("Ctrl+C"),
        b"\x04" => Some("Ctrl+D"),
        b"\x1a" => Some("Ctrl+Z"),
        b"\x08" => Some("Ctrl+H"),
        b"\x0c" => Some("Ctrl+L"),
        // Mouse — X10 format
        b if b.len() == 6 && b[0] == 0x1b && b[1] == b'[' && b[2] == b'M' => Some("Mouse(X10)"),
        // Mouse — SGR format (starts with ESC[<)
        b if b.len() >= 6 && b[0] == 0x1b && b[1] == b'[' && b[2] == b'<' => Some("Mouse(SGR)"),
        _ => None,
    }
}

struct RawMode;

impl RawMode {
    fn enable() -> Option<Self> {
        crossterm::terminal::enable_raw_mode().ok()?;
        Some(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}