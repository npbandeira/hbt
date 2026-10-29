//! Configuração persistente do cliente (servidor + token + fingerprint), como o
//! `ngrok config add-authtoken`: você configura uma vez e depois só digita
//! `hbt http 3000`.
//!
//! Prioridade de resolução: flag na linha de comando > variável de ambiente >
//! arquivo. (clap já resolve flag e env; aqui entra o arquivo como "último
//! recurso".)

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Porta de controle padrão do hbt-server, usada se o usuário digitar só o host.
const DEFAULT_PORT: u16 = 7000;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub server: Option<String>,
    pub token: Option<String>,
    /// SHA-256 do certificado do servidor (hex): o único certificado aceito.
    pub fingerprint: Option<String>,
}

/// `~/.config/hbt/config.toml` no Linux, equivalente nos outros SOs (crate `dirs`).
pub fn path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("não encontrei o diretório de configuração do usuário")?
        .join("hbt")
        .join("config.toml"))
}

/// Arquivo inexistente não é erro: devolve a configuração vazia.
pub fn load() -> Result<Config> {
    let path = path()?;
    match fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("configuração inválida em {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("lendo {}", path.display())),
    }
}

pub fn save(cfg: &Config) -> Result<PathBuf> {
    let path = path()?;
    fs::create_dir_all(path.parent().expect("path() sempre tem pai"))?;
    let text = toml::to_string_pretty(cfg)?;

    // O arquivo contém um segredo: criamos já com permissão 0600 (só o dono
    // lê). Criar e DEPOIS dar chmod deixaria uma janela em que ele é legível.
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .with_context(|| format!("gravando {}", path.display()))?;
    Ok(path)
}

/// `meu.vps` -> `meu.vps:7000`; `meu.vps:9000` fica como está.
pub fn with_default_port(server: &str) -> String {
    // IPv6 literal tem ':' mas vem entre colchetes: "[::1]" sem porta.
    let has_port = match server.rsplit_once(':') {
        Some((_, p)) => p.bytes().all(|b| b.is_ascii_digit()) && !p.is_empty(),
        None => false,
    };
    if has_port {
        server.to_owned()
    } else {
        format!("{server}:{DEFAULT_PORT}")
    }
}

/// Normaliza e valida o fingerprint: 64 dígitos hex, com ou sem `:`, em qualquer caixa.
pub fn parse_fingerprint(s: &str) -> Option<String> {
    let fp = s.trim().replace(':', "").to_ascii_lowercase();
    (fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit())).then_some(fp)
}

/// Mostra só o começo do token (não vaze segredos no terminal/histórico).
pub fn mask(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_normalized_and_validated() {
        let fp = "AB".repeat(32);
        assert_eq!(parse_fingerprint(&fp), Some("ab".repeat(32)));
        let colons = ["ab"; 32].join(":");
        assert_eq!(parse_fingerprint(&colons), Some("ab".repeat(32)));
        assert_eq!(parse_fingerprint("abc"), None);
        assert_eq!(parse_fingerprint(&"zz".repeat(32)), None);
    }

    #[test]
    fn default_port_is_added_only_when_missing() {
        assert_eq!(with_default_port("meu.vps"), "meu.vps:7000");
        assert_eq!(with_default_port("meu.vps:9000"), "meu.vps:9000");
        assert_eq!(with_default_port("1.2.3.4"), "1.2.3.4:7000");
        assert_eq!(with_default_port("[::1]"), "[::1]:7000");
        assert_eq!(with_default_port("[::1]:80"), "[::1]:80");
    }
}
