//! hbt-server: roda na nuvem (IP público). Aceita clientes na porta de
//! controle e abre portas públicas que redirecionam o tráfego pelo túnel.
//!
//! Três tipos de túnel: TCP e UDP (porta pública dedicada) e HTTP (uma porta
//! de entrada compartilhada; o `Host` da requisição escolhe o túnel).
//!
//! Fluxo (resumo; detalhes em docs/GUIDE.md):
//!
//!   cliente abre uma conexão QUIC na porta de controle (UDP), autentica e
//!   registra o túnel no primeiro stream; a mesma conexão carrega todo o tráfego.
//!
//!   usuário --> [porta pública no servidor]
//!                   |  servidor abre um stream QUIC novo (TCP/HTTP) ou manda um
//!                   |  datagrama QUIC (UDP), sem handshake
//!                   v
//!               cliente recebe o stream, conecta no serviço local e cola as pontas

mod accounts;
mod cli;
mod control;
mod db;
mod http_edge;
mod identity;
mod metrics;
mod serve;
mod state;
mod tls;
mod tokens;
mod tunnel;
mod web;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};
use db::Store;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Command::Token { cmd } => cli::token_admin(&Store::open(&cli.data_dir)?, cmd, &cli.data_dir),
        Command::User { cmd } => cli::user_admin(&Store::open(&cli.data_dir)?, cmd),
        Command::Fingerprint => {
            println!("{}", identity::load_or_create(&cli.data_dir)?.fingerprint());
            Ok(())
        }
        Command::Serve(args) => {
            let store = Store::open(&cli.data_dir)?;
            serve::serve(
                *args,
                store,
                identity::load_or_create(&cli.data_dir)?,
                &cli.data_dir,
            )
            .await
        }
    }
}
