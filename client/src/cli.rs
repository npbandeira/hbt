//! Argumentos da linha de comando, resolução de credenciais e `hbt configure`.

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use hbt_proto::*;

use crate::config;

#[derive(Parser)]
#[command(version, about = "hbt reverse tunnel client (estilo ngrok)")]
pub struct Cli {
    /// Servidor hbt, ex: tunnel.exemplo.com ou 1.2.3.4:7000 (porta padrão 7000).
    /// Se omitido, usa o valor salvo por `hbt configure`.
    #[arg(long, env = "HBT_SERVER", global = true)]
    pub server: Option<String>,

    /// Segredo compartilhado com o servidor (prefira `hbt configure` ou a env).
    #[arg(long, env = "HBT_TOKEN", hide_env_values = true, global = true)]
    pub token: Option<String>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Salva servidor e token para não precisar repeti-los (sem argumentos: mostra o atual).
    Configure,
    /// Expõe um servidor web local numa URL pública, ex: `hbt http 3000`.
    Http(HttpArgs),
    /// Expõe um serviço TCP local numa porta pública, ex: `hbt tcp 22`.
    Tcp(PortArgs),
    /// Expõe um serviço UDP local numa porta pública, ex: `hbt udp 51820`.
    Udp(PortArgs),
}

#[derive(ClapArgs)]
pub struct HttpArgs {
    /// O que expor: só a porta (`3000`, vira 127.0.0.1:3000) ou `host:porta`.
    pub target: String,
    /// Subdomínio desejado (padrão: sorteado pelo servidor).
    #[arg(long)]
    pub subdomain: Option<String>,
}

#[derive(ClapArgs)]
pub struct PortArgs {
    /// O que expor: só a porta (`22`, vira 127.0.0.1:22) ou `host:porta`.
    pub target: String,
    /// Porta pública desejada (padrão: sorteada pelo servidor).
    #[arg(long)]
    pub remote_port: Option<u16>,
}

/// Servidor + token já resolvidos (flag > env > arquivo).
pub struct Conn {
    pub server: String,
    /// Parte pública do token: diz ao servidor "quem sou eu".
    pub token_id: String,
    /// Chave derivada do segredo, usada para assinar o desafio.
    pub key: [u8; 32],
}

const BAD_TOKEN: &str = "token inválido: o formato é <id>.<segredo> (gerado por `hbt-server token create`)";

/// O que o usuário pediu, já traduzido: o que registrar e para onde encaminhar.
pub struct Plan {
    pub spec: TunnelSpec,
    pub local: String,
}

impl Plan {
    /// `None` para comandos que não abrem túnel (configure).
    pub fn from_cmd(cmd: &Cmd) -> Option<Plan> {
        Some(match cmd {
            Cmd::Configure => return None,
            Cmd::Http(a) => Plan {
                spec: TunnelSpec::Http {
                    subdomain: a.subdomain.clone(),
                },
                local: local_addr(&a.target),
            },
            Cmd::Tcp(a) => Plan {
                spec: TunnelSpec::Tcp {
                    remote_port: a.remote_port,
                },
                local: local_addr(&a.target),
            },
            Cmd::Udp(a) => Plan {
                spec: TunnelSpec::Udp {
                    remote_port: a.remote_port,
                },
                local: local_addr(&a.target),
            },
        })
    }
}

/// `3000` => `127.0.0.1:3000` (só dígitos = porta local); caso contrário usa como veio.
pub fn local_addr(target: &str) -> String {
    if !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()) {
        format!("127.0.0.1:{target}")
    } else {
        target.to_owned()
    }
}

/// Usa as flags globais `--server`/`--token` do próprio `Cli`.
pub fn configure(cli: &Cli) -> Result<()> {
    let mut cfg = config::load()?;
    if cli.server.is_none() && cli.token.is_none() {
        println!("arquivo : {}", config::path()?.display());
        println!("server  : {}", cfg.server.as_deref().unwrap_or("(não definido)"));
        println!(
            "token   : {}",
            cfg.token.as_deref().map_or("(não definido)".into(), config::mask)
        );
        return Ok(());
    }
    if let Some(s) = &cli.server {
        cfg.server = Some(config::with_default_port(s));
    }
    if let Some(t) = &cli.token {
        // Falha cedo: melhor recusar agora do que descobrir na hora de conectar.
        parse_token(t).context(BAD_TOKEN)?;
        cfg.token = Some(t.trim().to_owned());
    }
    let path = config::save(&cfg)?;
    println!("configuração salva em {}", path.display());
    Ok(())
}

/// Combina flag/env (já feitos pelo clap) com o arquivo.
pub fn resolve(cli: &Cli) -> Result<Conn> {
    let file = config::load()?;
    let server = cli.server.clone().or(file.server).context(
        "servidor não definido: rode `hbt configure --server HOST --token TOKEN` (ou use --server)",
    )?;
    let token =
        cli.token.clone().or(file.token).context(
            "token não definido: rode `hbt configure --server HOST --token TOKEN` (ou use --token)",
        )?;
    let (token_id, secret) = parse_token(&token).context(BAD_TOKEN)?;
    Ok(Conn {
        server: config::with_default_port(&server),
        token_id: token_id.to_owned(),
        key: derive_key(secret),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_port_means_localhost() {
        assert_eq!(local_addr("3000"), "127.0.0.1:3000");
    }

    #[test]
    fn explicit_targets_are_kept() {
        assert_eq!(local_addr("192.168.0.10:8080"), "192.168.0.10:8080");
        assert_eq!(local_addr("meu-host:22"), "meu-host:22");
        assert_eq!(local_addr(""), "");
    }
}
