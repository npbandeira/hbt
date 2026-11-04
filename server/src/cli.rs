//! Argumentos da linha de comando e administração de tokens.

use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use crate::accounts;
use crate::db::Store;
use crate::identity;
use crate::tokens;

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
    Serve(Box<ServeArgs>),
    /// Gerencia as contas do site (e-mail e senha; valem imediatamente).
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
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

#[derive(Subcommand)]
pub enum UserCmd {
    /// Cria uma conta. A senha é gerada e mostrada UMA vez.
    Add { email: String },
    /// Lista as contas (sem senhas).
    List,
    /// Gera uma senha nova (mostrada UMA vez).
    Reset { email_or_id: String },
    /// Bloqueia a conta e revoga TODOS os tokens dela (túneis caem em ~10s).
    Disable { email_or_id: String },
    /// Libera uma conta bloqueada (os tokens revogados não voltam).
    Enable { email_or_id: String },
    /// Apaga a conta e os tokens dela.
    Remove { email_or_id: String },
}

/// Quem pode criar conta no site.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Signup {
    /// Qualquer pessoa; o abuso é limitado por cotas e pelo `user disable`.
    Open,
    /// Só contas criadas pelo admin com `hbt-server user add`.
    Closed,
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
    /// exige root ou `setcap cap_net_bind_service=+ep`. Com --https-bind e o
    /// certificado pronto, ela só redireciona para https.
    #[arg(long, env = "HBT_HTTP_BIND", default_value = "0.0.0.0:80")]
    pub http_bind: String,

    /// Onde escutar HTTPS público, ex: 0.0.0.0:443 (usado com --domain). O
    /// certificado cobre `<domain>` (o site, se --web-bind estiver ligado) e
    /// `*.<domain>` (os túneis); fica em `<data-dir>/tls/{cert,key}.pem`.
    #[arg(long, env = "HBT_HTTPS_BIND")]
    pub https_bind: Option<String>,

    /// Token do DuckDNS: com ele o servidor emite e renova sozinho o
    /// certificado no Let's Encrypt (o --domain precisa ser `<nome>.duckdns.org`).
    /// Sem ele, ponha o certificado em `<data-dir>/tls/` por fora.
    #[arg(long, env = "HBT_DUCKDNS_TOKEN", hide_env_values = true)]
    pub duckdns_token: Option<String>,

    /// E-mail de contato da conta no Let's Encrypt (avisos de expiração). Opcional.
    #[arg(long, env = "HBT_ACME_EMAIL")]
    pub acme_email: Option<String>,

    /// Usa o ambiente de testes do Let's Encrypt (certificado não confiável,
    /// limites folgados). Para experimentar a configuração sem gastar a cota.
    #[arg(long, env = "HBT_ACME_STAGING")]
    pub acme_staging: bool,

    /// Onde servir métricas Prometheus (`GET /metrics`), ex: 127.0.0.1:9477.
    /// Sem isso, desligado. Não há autenticação: não exponha à internet.
    #[arg(long, env = "HBT_METRICS_BIND")]
    pub metrics_bind: Option<String>,

    /// Onde servir o site (landing, cadastro, painel de tokens), ex:
    /// 127.0.0.1:8081. Sem isso, o site fica desligado. Ele tem senha e token:
    /// sirva-o por HTTPS, com --https-bind (aparece no domínio raiz) ou com um
    /// proxy (Caddy/nginx) na frente.
    #[arg(long, env = "HBT_WEB_BIND")]
    pub web_bind: Option<String>,

    /// URL pública do site, ex: `https://meuhbt.duckdns.org`. O host dela vai no
    /// comando de configuração que o painel mostra. Sem isso usa o `Host` da página.
    #[arg(long, env = "HBT_WEB_URL")]
    pub web_url: Option<String>,

    /// Quem pode criar conta no site.
    #[arg(long, env = "HBT_SIGNUP", value_enum, default_value = "open")]
    pub signup: Signup,

    /// Quantos tokens cada conta do site pode ter.
    #[arg(long, env = "HBT_MAX_TOKENS_PER_USER", default_value_t = 5)]
    pub max_tokens_per_user: usize,

    /// Túneis simultâneos por token (cada um ocupa uma porta pública).
    #[arg(long, env = "HBT_MAX_TUNNELS_PER_TOKEN", default_value_t = 2)]
    pub max_tunnels_per_token: usize,

    /// Quantos subdomínios cada conta do site pode reservar (0 desliga as reservas).
    #[arg(long, env = "HBT_MAX_RESERVED_PER_USER", default_value_t = 3)]
    pub max_reserved_per_user: usize,

    /// Dias sem túnel aberto até uma reserva de subdomínio ser liberada.
    #[arg(long, env = "HBT_RESERVATION_DAYS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub reservation_days: u64,
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

/// A linha que o usuário cola no terminal para configurar o cliente `hbt`.
pub fn configure_command(server: &str, token: &str, fingerprint: &str) -> String {
    format!("hbt configure --server {server} --token {token} --fingerprint {fingerprint}")
}

pub fn user_admin(store: &Store, cmd: UserCmd) -> Result<()> {
    match cmd {
        UserCmd::Add { email } => {
            anyhow::ensure!(accounts::valid_email(&email), "e-mail inválido: {email}");
            let password = accounts::generate();
            let user = store.add_user(&email, &accounts::hash(&password)?)?;
            println!("Conta criada: {} ({})\n", user.email, user.id);
            println!("  senha: {password}\n");
            println!("Guarde a senha agora: ela não será mostrada de novo.");
        }
        UserCmd::List => {
            let users = store.list_users()?;
            if users.is_empty() {
                println!("(nenhuma conta) crie com: hbt-server user add <e-mail>");
            }
            for (u, tokens) in users {
                println!(
                    "{:<18} {:<32} {:>3} tokens  {:<9} {}",
                    u.id,
                    u.email,
                    tokens,
                    if u.disabled { "bloqueada" } else { "ativa" },
                    tokens::format_date(u.created_at)
                );
            }
        }
        UserCmd::Reset { email_or_id } => {
            let password = accounts::generate();
            let user = store.set_password(&email_or_id, &accounts::hash(&password)?)?;
            println!("Nova senha de {}:\n\n  {password}\n", user.email);
            println!("Guarde agora: ela não será mostrada de novo.");
        }
        UserCmd::Disable { email_or_id } => {
            let (u, revoked) = store.set_disabled(&email_or_id, true)?;
            println!("Conta {} bloqueada; {revoked} tokens revogados.", u.email);
        }
        UserCmd::Enable { email_or_id } => {
            let (u, _) = store.set_disabled(&email_or_id, false)?;
            println!("Conta {} liberada.", u.email);
        }
        UserCmd::Remove { email_or_id } => {
            let (u, revoked) = store.remove_user(&email_or_id)?;
            println!("Conta {} apagada; {revoked} tokens revogados.", u.email);
        }
    }
    Ok(())
}

pub fn token_admin(store: &Store, cmd: TokenCmd, data_dir: &Path) -> Result<()> {
    match cmd {
        TokenCmd::Create {
            name,
            ports,
            subdomains,
            quiet,
        } => {
            let token = store.create(&name, ports, subdomains, None, None)?;
            if quiet {
                println!("{token}");
            } else {
                println!("Token '{name}' criado. Guarde-o agora: ele não será mostrado de novo.\n");
                println!("  {token}\n");
                let fingerprint = identity::load_or_create(data_dir)?.fingerprint();
                println!(
                    "No cliente:  {}",
                    configure_command("SEU_SERVIDOR", &token, &fingerprint)
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
