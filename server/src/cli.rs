//! Argumentos da linha de comando e administração de tokens.

use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::identity;
use crate::tokens::{self, TokenStore};

#[derive(Parser)]
#[command(version, about = "hbt reverse tunnel server")]
pub struct Cli {
    /// Diretório de dados (guarda tokens.json e a identidade TLS). Precisa ser gravável.
    #[arg(long, env = "HBT_DATA_DIR", default_value = "/var/lib/hbt", global = true)]
    pub data_dir: PathBuf,

    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Roda o servidor.
    Serve(ServeArgs),
    /// Mostra o fingerprint do certificado do servidor (gera o certificado se não existir).
    /// É o valor que o cliente confere em `hbt configure --fingerprint`.
    Fingerprint,
    /// Gerencia os tokens dos clientes (valem imediatamente, sem reiniciar).
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
}

#[derive(Subcommand)]
pub enum TokenCmd {
    /// Cria um token. Ele é mostrado UMA vez; o servidor não guarda o segredo.
    Create {
        /// Nome do cliente/dispositivo, ex: notebook-ana
        name: String,
        /// Limita as portas TCP/UDP que este token pode abrir, ex: 20000-20100
        #[arg(long, value_parser = parse_range)]
        ports: Option<RangeInclusive<u16>>,
        /// Limita os subdomínios HTTP (separados por vírgula), ex: ana,ana-dev
        #[arg(long, value_delimiter = ',')]
        subdomains: Option<Vec<String>>,
        /// Imprime só o token (para scripts).
        #[arg(short, long)]
        quiet: bool,
    },
    /// Lista os tokens (sem segredos).
    List,
    /// Revoga um token por nome ou id; sessões ativas caem em até ~10s.
    Revoke { name_or_id: String },
}

#[derive(Parser)]
pub struct ServeArgs {
    /// Endereço da porta de controle (UDP/QUIC; onde os clientes se conectam).
    #[arg(long, default_value = "0.0.0.0:7000")]
    pub control_bind: String,

    /// IP em que as portas públicas dos túneis são abertas.
    #[arg(long, default_value = "0.0.0.0")]
    pub tunnel_ip: IpAddr,

    /// Faixa de portas que clientes podem pedir, ex: 20000-30000.
    #[arg(long, default_value = "20000-30000", value_parser = parse_range)]
    pub port_range: RangeInclusive<u16>,

    /// Domínio base dos túneis HTTP: `abc.<domain>` vai para o túnel `abc`.
    /// Sem isso, túneis HTTP ficam desabilitados.
    #[arg(long, env = "HBT_DOMAIN")]
    pub domain: Option<String>,

    /// Onde escutar requisições HTTP públicas (usado com --domain). A porta 80
    /// exige root ou `setcap cap_net_bind_service=+ep`. Para HTTPS, ponha um
    /// proxy com TLS (Caddy/nginx) na frente e use p.ex. 127.0.0.1:8080.
    #[arg(long, env = "HBT_HTTP_BIND", default_value = "0.0.0.0:80")]
    pub http_bind: String,

    /// Onde servir métricas Prometheus (`GET /metrics`), ex: 127.0.0.1:9477.
    /// Sem isso, desligado. Não há autenticação: não exponha à internet.
    #[arg(long, env = "HBT_METRICS_BIND")]
    pub metrics_bind: Option<String>,
}

fn parse_range(s: &str) -> Result<RangeInclusive<u16>, String> {
    let (a, b) = s.split_once('-').ok_or("use o formato INICIO-FIM")?;
    let (a, b): (u16, u16) = (
        a.parse().map_err(|e| format!("{e}"))?,
        b.parse().map_err(|e| format!("{e}"))?,
    );
    if a > b {
        return Err("INICIO deve ser <= FIM".into());
    }
    Ok(a..=b)
}

pub fn token_admin(store: &TokenStore, cmd: TokenCmd, data_dir: &Path) -> Result<()> {
    match cmd {
        TokenCmd::Create {
            name,
            ports,
            subdomains,
            quiet,
        } => {
            let token = store.create(&name, ports, subdomains)?;
            if quiet {
                println!("{token}");
            } else {
                println!("Token '{name}' criado. Guarde-o agora: ele não será mostrado de novo.\n");
                println!("  {token}\n");
                let fingerprint = identity::load_or_create(data_dir)?.fingerprint();
                println!(
                    "No cliente:  hbt configure --server SEU_SERVIDOR --token {token} --fingerprint {fingerprint}"
                );
            }
        }
        TokenCmd::List => tokens::print_table(&store.load()?),
        TokenCmd::Revoke { name_or_id } => {
            let r = store.revoke(&name_or_id)?;
            println!("Token '{}' ({}) revogado.", r.name, r.id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_range_accepts_valid_ranges() {
        assert_eq!(parse_range("20000-30000").unwrap(), 20000..=30000);
        assert_eq!(parse_range("80-80").unwrap(), 80..=80);
    }

    #[test]
    fn parse_range_rejects_bad_input() {
        assert!(parse_range("20000").is_err());
        assert!(parse_range("30000-20000").is_err());
        assert!(parse_range("a-b").is_err());
        assert!(parse_range("1-70000").is_err());
    }
}
