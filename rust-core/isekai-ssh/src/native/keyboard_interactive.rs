//! Console glue for `keyboard-interactive` ("PAM"/OTP/2FA-style)
//! authentication on the native SSH client path — servers that don't
//! negotiate plain `password` (many PAM-backed `sshd` configs, and
//! OTP/2FA-gated ones) require this instead. Split from `native/connect.rs`
//! so the prompt-answering logic itself (`console_responder`) has no
//! dependency on `russh`/a live session — the actual round-tripping lives in
//! `russh_stream_session::authenticate_keyboard_interactive`, which this
//! crate's `connect.rs` calls with this module's responder (or a
//! silent-mode-refusing one — see `run_authenticated_session`'s
//! `kbi_responder` construction, the same seam `console::prompt_passphrase`
//! uses for encrypted identity files).

use russh_stream_session::KeyboardInteractivePrompt;

/// Answers one round of server prompts by reading from the console: no local
/// echo for a prompt the server marked sensitive (`echo == false` — a
/// password, an OTP code), a plain visible line otherwise. Uses `rpassword`
/// for the no-echo case, same as [`super::console::prompt_passphrase`] (same
/// "don't reimplement a solved problem" stance as this crate takes for
/// `crossterm`'s raw mode).
///
/// **Only meaningfully exercised against a real interactive terminal** — see
/// [`super::console::prompt_passphrase`]'s identical caveat; the
/// round-looping logic this feeds is unit-tested in `russh-stream-session`
/// and `native::connect` via a fake responder instead.
pub(crate) fn console_responder(prompts: &[KeyboardInteractivePrompt]) -> Vec<String> {
    prompts
        .iter()
        .map(|p| {
            if p.echo {
                use std::io::Write as _;
                eprint!("{}", p.prompt);
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                line.trim_end_matches(['\r', '\n']).to_string()
            } else {
                rpassword::prompt_password(&p.prompt).unwrap_or_default()
            }
        })
        .collect()
}
