//! Contadores e painel de status no terminal (conexões, bytes, estado) e o log de requisições.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::time::Duration;

// shortcut: estado global, só há um túnel por processo; trocar por Arc se um dia houver vários.
static OPEN: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static BYTES_IN: AtomicU64 = AtomicU64::new(0);
static BYTES_OUT: AtomicU64 = AtomicU64::new(0);
static STATE: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy)]
// 0 (estado inicial) = "connecting", nunca atribuído.
pub enum State {
    Online = 1,
    Reconnecting = 2,
}

pub fn set_state(s: State) {
    STATE.store(s as u8, Relaxed);
}

/// Conta a conexão enquanto o guard viver.
pub struct ConnGuard;

pub fn conn_guard() -> ConnGuard {
    OPEN.fetch_add(1, Relaxed);
    TOTAL.fetch_add(1, Relaxed);
    ConnGuard
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Relaxed);
    }
}

/// `inn`: bytes vindos do usuário público; `out`: bytes devolvidos a ele.
pub fn add_bytes(inn: u64, out: u64) {
    BYTES_IN.fetch_add(inn, Relaxed);
    BYTES_OUT.fetch_add(out, Relaxed);
}

// Cores do site (`--c-accent`, amarelo e vermelho de app.css). Sem cor em pipe nem com NO_COLOR.
pub const BRAND: &str = "1;38;2;176;41;143";
pub const YELLOW: &str = "38;2;255;192;19";
pub const RED: &str = "38;2;220;60;60";
pub const GREEN: &str = "38;2;80;190;120";
pub const DIM: &str = "2";
pub const BOLD: &str = "1";

fn color(tty: bool) -> bool {
    tty && std::env::var_os("NO_COLOR").is_none()
}

/// Pinta para o stdout (banner e log).
pub fn paint(code: &str, s: &str) -> String {
    paint_if(color(std::io::stdout().is_terminal()), code, s)
}

fn paint_if(on: bool, code: &str, s: &str) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

/// Cor do status HTTP: 2xx verde, 3xx normal, 4xx amarelo, 5xx vermelho.
pub fn status_color(code: u16) -> &'static str {
    match code {
        200..=299 => GREEN,
        400..=499 => YELLOW,
        500..=599 => RED,
        _ => DIM,
    }
}

fn human(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", U[i])
    }
}

/// Imprime uma linha de log sem deixar resto do painel na mesma linha.
pub fn log(line: &str) {
    let clear = if std::io::stderr().is_terminal() {
        "\r\x1b[2K"
    } else {
        ""
    };
    // `println!` daria panic se o stdout fechou (pipe sem leitor) e derrubaria a conexão.
    let _ = writeln!(std::io::stdout(), "{clear}{line}");
}

/// Painel de uma linha, reescrito a cada segundo. Só aparece num terminal:
/// em pipe/arquivo ficaria só ruído.
pub fn spawn_status() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let on = color(true);
            let (dot, state) = match STATE.load(Relaxed) {
                1 => (BRAND, "online"),
                2 => (YELLOW, "reconnecting"),
                _ => (DIM, "connecting"),
            };
            let rest = format!(
                "{state} · {} conn ({} total) · ↓ {} ↑ {}",
                OPEN.load(Relaxed),
                TOTAL.load(Relaxed),
                human(BYTES_IN.load(Relaxed)),
                human(BYTES_OUT.load(Relaxed)),
            );
            eprint!(
                "\r\x1b[2K  {} {}",
                paint_if(on, dot, "●"),
                paint_if(on, DIM, &rest)
            );
            let _ = std::io::stderr().flush();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(1023), "1023B");
        assert_eq!(human(1536), "1.5KB");
        assert_eq!(human(5 * 1024 * 1024), "5.0MB");
    }
}
