//! hbt: CLI do cliente. Roda na máquina que tem o serviço local e expõe
//! esse serviço através de uma porta pública do `hbt-server`.
//!
//!   hbt configure --server meu.vps --token ID.SEGREDO --fingerprint FP   (uma vez só)
//!   hbt http 3000          # URL pública para o site local na porta 3000
//!   hbt tcp 22             # porta pública (sorteada) para o ssh local
//!   hbt udp 51820          # idem para UDP
//!
//! "Reverse" tunnel: quem inicia a conexão é o cliente (saída), então funciona
//! atrás de NAT/firewall sem abrir nenhuma porta de entrada. É UMA conexão QUIC
//! só: cada usuário público TCP/HTTP chega como um stream dentro dela e cada
//! datagrama UDP como um datagrama QUIC (veja proto/src/mux.rs).

mod cli;
mod config;
mod forward;
mod session;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use cli::{configure, resolve, Cli, Plan};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        // Padrão "warn": a saída principal é o banner do túnel, não logs.
        // Use RUST_LOG=info ou debug para ver o que acontece por dentro.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let Some(plan) = Plan::from_cmd(&cli.cmd) else {
        return configure(&cli);
    };
    let conn = resolve(&cli)?;

    // Ctrl-C encerra limpo. `select!` roda os dois futures e termina quando
    // o PRIMEIRO completar, descartando o outro.
    tokio::select! {
        r = session::reconnect_loop(&conn, &plan) => r,
        _ = tokio::signal::ctrl_c() => { info!("encerrando"); Ok(()) }
    }
}
