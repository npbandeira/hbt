//! hbt: CLI do cliente. Roda na máquina que tem o serviço local e expõe
//! esse serviço através de uma porta pública do `hbt-server`.
//!
//!   hbt configure --server meu.vps --token ID.SEGREDO    (uma vez só)
//!   hbt http 3000          # URL pública para o site local na porta 3000
//!   hbt tcp 22             # porta pública (sorteada) para o ssh local
//!   hbt udp 51820          # idem para UDP
//!
//! "Reverse" tunnel: quem inicia TODAS as conexões é o cliente (saída), então
//! funciona atrás de NAT/firewall sem abrir nenhuma porta de entrada.

mod config;

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use hbt_proto::*;
use tokio::io::{copy_bidirectional, BufReader};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::{interval, timeout};
use tracing::{debug, error, info, warn};

/// Sem tráfego do lado local por este tempo, a "sessão" UDP é descartada.
const UDP_IDLE: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(version, about = "hbt reverse tunnel client (estilo ngrok)")]
struct Cli {
    /// Servidor hbt, ex: tunnel.exemplo.com ou 1.2.3.4:7000 (porta padrão 7000).
    /// Se omitido, usa o valor salvo por `hbt configure`.
    #[arg(long, env = "HBT_SERVER", global = true)]
    server: Option<String>,

    /// Segredo compartilhado com o servidor (prefira `hbt configure` ou a env).
    #[arg(long, env = "HBT_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
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
struct HttpArgs {
    /// O que expor: só a porta (`3000`, vira 127.0.0.1:3000) ou `host:porta`.
    target: String,
    /// Subdomínio desejado (padrão: sorteado pelo servidor).
    #[arg(long)]
    subdomain: Option<String>,
}

#[derive(ClapArgs)]
struct PortArgs {
    /// O que expor: só a porta (`22`, vira 127.0.0.1:22) ou `host:porta`.
    target: String,
    /// Porta pública desejada (padrão: sorteada pelo servidor).
    #[arg(long)]
    remote_port: Option<u16>,
}

/// Servidor + token já resolvidos (flag > env > arquivo).
struct Conn {
    server: String,
    /// Parte pública do token: diz ao servidor "quem sou eu".
    token_id: String,
    /// Chave derivada do segredo, usada para assinar o desafio.
    key: [u8; 32],
}

const BAD_TOKEN: &str = "token inválido: o formato é <id>.<segredo> (gerado por `hbt-server token create`)";

/// O que o usuário pediu, já traduzido: o que registrar e para onde encaminhar.
struct Plan {
    spec: TunnelSpec,
    local: String,
}

impl Plan {
    /// `None` para comandos que não abrem túnel (configure).
    fn from_cmd(cmd: &Cmd) -> Option<Plan> {
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
fn local_addr(target: &str) -> String {
    if !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()) {
        format!("127.0.0.1:{target}")
    } else {
        target.to_owned()
    }
}

/// Usa as flags globais `--server`/`--token` do próprio `Cli`.
fn configure(cli: &Cli) -> Result<()> {
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
fn resolve(cli: &Cli) -> Result<Conn> {
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

/// Erro que não adianta tentar de novo (ex.: token errado).
#[derive(Debug)]
struct Fatal(String);
impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Fatal {}

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
        r = reconnect_loop(&conn, &plan) => r,
        _ = tokio::signal::ctrl_c() => { info!("encerrando"); Ok(()) }
    }
}

/// Saída principal do programa, no estilo do ngrok.
fn print_banner(conn: &Conn, plan: &Plan, public: &Public) {
    let url = match public {
        Public::Url(u) => u.clone(),
        Public::Port(p) => {
            let host = conn
                .server
                .rsplit_once(':')
                .map_or(conn.server.as_str(), |(h, _)| h);
            let scheme = if matches!(plan.spec, TunnelSpec::Udp { .. }) {
                "udp"
            } else {
                "tcp"
            };
            format!("{scheme}://{host}:{p}")
        }
    };
    println!("\nhbt                                            (Ctrl+C para sair)\n");
    println!("Forwarding    {url}  ->  {}\n", plan.local);
}

/// Reconecta para sempre com *exponential backoff*: espera 1s, 2s, 4s... até
/// 30s entre tentativas. Sem isso, um servidor fora do ar seria martelado em
/// loop apertado (e vários clientes juntos fariam um "thundering herd").
async fn reconnect_loop(conn: &Conn, plan: &Plan) -> Result<()> {
    let mut delay = Duration::from_secs(1);
    loop {
        let mut registered = false;
        match run_session(conn, plan, &mut registered).await {
            Ok(()) => warn!("sessão terminou"),
            Err(e) => {
                // `downcast_ref` recupera o tipo concreto escondido no anyhow::Error.
                if let Some(f) = e.downcast_ref::<Fatal>() {
                    error!("erro fatal: {f}");
                    return Err(e);
                }
                warn!("sessão caiu: {e:#}");
            }
        }
        if registered {
            delay = Duration::from_secs(1); // estava saudável: recomeça rápido
        }
        warn!("reconectando em {delay:?}");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn run_session(conn: &Conn, plan: &Plan, registered: &mut bool) -> Result<()> {
    let mut ctl = TcpStream::connect(&conn.server)
        .await
        .with_context(|| format!("conectando em {}", conn.server))?;
    write_msg(
        &mut ctl,
        &Hello::Control {
            token_id: conn.token_id.clone(),
        },
    )
    .await?;

    let msg: ServerMsg = timeout(Duration::from_secs(5), read_msg(&mut ctl))
        .await
        .context("timeout")??;
    let ServerMsg::Challenge(nonce) = msg else {
        bail!("esperava Challenge")
    };
    write_msg(&mut ctl, &ClientMsg::Auth(auth_response(&conn.key, &nonce))).await?;

    match timeout(Duration::from_secs(5), read_msg(&mut ctl))
        .await
        .context("timeout")??
    {
        ServerMsg::AuthOk => {}
        ServerMsg::Error(e) => return Err(Fatal(e).into()),
        _ => bail!("resposta inesperada à autenticação"),
    }

    write_msg(&mut ctl, &ClientMsg::Register(plan.spec.clone())).await?;
    match timeout(Duration::from_secs(5), read_msg(&mut ctl))
        .await
        .context("timeout")??
    {
        ServerMsg::Registered { public } => {
            print_banner(conn, plan, &public);
            *registered = true;
        }
        // Pode ser transitório (servidor ainda não notou que a sessão antiga
        // morreu e a porta segue ocupada), então NÃO é Fatal: tentamos de novo.
        ServerMsg::Error(e) => bail!("servidor recusou: {e}"),
        _ => bail!("resposta inesperada ao registro"),
    }

    let (rd, mut wr) = ctl.into_split();
    let mut rd = BufReader::new(rd);

    // Heartbeat do cliente => o servidor detecta quando morremos sem FIN
    // (cabo puxado, NAT que esqueceu a conexão...).
    // CUIDADO: o nome `_hb` (com nome) mantém o guard vivo até o fim da
    // função. Se fosse só `_`, o valor seria destruído NA HORA e o task abortado.
    let _hb = AbortOnDrop(tokio::spawn(async move {
        let mut tick = interval(HEARTBEAT_EVERY);
        loop {
            tick.tick().await;
            if write_msg(&mut wr, &ClientMsg::Heartbeat).await.is_err() {
                return;
            }
        }
    }));

    loop {
        // Se o timeout estourar, `?` sai da função: a sessão é descartada
        // inteira, então perder bytes de uma leitura cancelada não importa.
        let msg: ServerMsg = timeout(HEARTBEAT_TIMEOUT, read_msg(&mut rd))
            .await
            .context("servidor sem heartbeat")??;
        match msg {
            ServerMsg::NewConn(id) => {
                let (server, local) = (conn.server.clone(), plan.local.clone());
                tokio::spawn(async move {
                    if let Err(e) = tcp_data(server, id, local).await {
                        warn!("conexão TCP: {e:#}");
                    }
                });
            }
            ServerMsg::NewUdpStream(id) => {
                let (server, local) = (conn.server.clone(), plan.local.clone());
                tokio::spawn(async move {
                    if let Err(e) = udp_data(server, id, local).await {
                        debug!("stream UDP: {e:#}");
                    }
                });
            }
            ServerMsg::Heartbeat => {}
            ServerMsg::Error(e) => bail!("erro do servidor: {e}"),
            _ => bail!("mensagem inesperada"),
        }
    }
}

/// Uma conexão de usuário: abre o lado de dados e cola no serviço local.
/// Erros ao montar a conexão sobem (viram `warn`); erros da cópia depois disso
/// são o usuário/serviço fechando e ficam em `debug`.
async fn tcp_data(server: String, id: ConnId, local: String) -> Result<()> {
    // Cada conexão pública abre uma conexão de dados nova: sob muitas conexões
    // curtas, as portas efêmeras desta máquina podem acabar (TIME_WAIT).
    let mut data = TcpStream::connect(&server)
        .await
        .with_context(|| format!("abrindo conexão de dados com {server}"))?;
    write_msg(&mut data, &Hello::Accept(id)).await?;
    // Conectamos no serviço local DEPOIS de aceitar no servidor: se o serviço
    // local estiver fora, o erro fecha `data` na hora e o usuário público é
    // desconectado rápido, em vez de ficar pendurado até o timeout.
    let mut svc = TcpStream::connect(&local)
        .await
        .with_context(|| format!("serviço local {local} indisponível"))?;
    let _ = (data.set_nodelay(true), svc.set_nodelay(true));
    if let Err(e) = copy_bidirectional(&mut data, &mut svc).await {
        debug!("conexão TCP: {e}");
    }
    Ok(())
}

type Sessions = Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>>;

/// Túnel UDP. Cada "peer" público ganha um socket UDP local próprio, para o
/// serviço local enxergar origens distintas e as respostas voltarem ao peer certo.
async fn udp_data(server: String, id: ConnId, local: String) -> Result<()> {
    let local_addr = tokio::net::lookup_host(&local)
        .await?
        .next()
        .with_context(|| format!("não resolvi {local}"))?;

    let mut data = TcpStream::connect(&server).await?;
    write_msg(&mut data, &Hello::Accept(id)).await?;
    let _ = data.set_nodelay(true);
    let (rd, mut wr) = data.into_split();
    let mut rd = BufReader::new(rd);

    // Respostas do serviço local -> canal -> UM task escritor no stream.
    let (out_tx, mut out_rx) = mpsc::channel::<UdpFrame>(256);
    let _writer = AbortOnDrop(tokio::spawn(async move {
        while let Some(f) = out_rx.recv().await {
            if write_msg(&mut wr, &f).await.is_err() {
                return;
            }
        }
    }));

    let sessions: Sessions = Arc::default();
    loop {
        let frame: UdpFrame = read_msg(&mut rd).await?;

        let existing = sessions.lock().unwrap().get(&frame.peer).cloned();
        let sock = match existing {
            Some(s) => s,
            None => {
                let bind = if local_addr.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                };
                let s = Arc::new(UdpSocket::bind(bind).await?);
                // `connect` em UDP não abre conexão: só fixa o destino padrão
                // e filtra datagramas de outras origens.
                s.connect(local_addr).await?;
                sessions.lock().unwrap().insert(frame.peer, s.clone());
                tokio::spawn(local_to_peer(
                    s.clone(),
                    frame.peer,
                    out_tx.clone(),
                    sessions.clone(),
                ));
                s
            }
        };
        if let Err(e) = sock.send(&frame.data).await {
            // Ex.: ConnectionRefused quando nada escuta (chega via ICMP).
            debug!("envio ao serviço local falhou: {e}");
        }
    }
}

async fn local_to_peer(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    out: mpsc::Sender<UdpFrame>,
    sessions: Sessions,
) {
    let mut buf = vec![0u8; 65535];
    // `while let` com padrão que só casa em sucesso: erro de I/O ou ociosidade
    // (Elapsed) encerram o loop e liberam o socket.
    while let Ok(Ok(n)) = timeout(UDP_IDLE, sock.recv(&mut buf)).await {
        let frame = UdpFrame {
            peer,
            data: buf[..n].to_vec(),
        };
        if out.send(frame).await.is_err() {
            break;
        }
    }
    sessions.lock().unwrap().remove(&peer);
}
