//! hbt: CLI do cliente. Roda na máquina que tem o serviço local e expõe
//! esse serviço através de uma porta pública do `hbt-server`.
//!
//!   hbt --server meu.vps:7000 tcp --local 127.0.0.1:22 --remote-port 2222
//!   hbt --server meu.vps:7000 udp --local 127.0.0.1:51820 --remote-port 51820
//!
//! "Reverse" tunnel: quem inicia TODAS as conexões é o cliente (saída), então
//! funciona atrás de NAT/firewall sem abrir nenhuma porta de entrada.

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
#[command(version, about = "hbt reverse tunnel client")]
struct Cli {
    /// Endereço de controle do servidor, ex: meu.vps:7000
    #[arg(long, env = "HBT_SERVER")]
    server: String,

    /// Segredo compartilhado com o servidor.
    #[arg(long, env = "HBT_TOKEN", hide_env_values = true)]
    token: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Expõe um serviço TCP local (ssh, http, ...).
    Tcp(Tunnel),
    /// Expõe um serviço UDP local (wireguard, dns, jogos, ...).
    Udp(Tunnel),
}

#[derive(ClapArgs, Clone)]
struct Tunnel {
    /// Serviço local a expor, ex: 127.0.0.1:22
    #[arg(long)]
    local: String,
    /// Porta que será aberta no servidor.
    #[arg(long)]
    remote_port: u16,
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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let (proto, tunnel) = match &cli.cmd {
        Cmd::Tcp(t) => (Proto::Tcp, t.clone()),
        Cmd::Udp(t) => (Proto::Udp, t.clone()),
    };

    // Ctrl-C encerra limpo. `select!` roda os dois futures e termina quando
    // o PRIMEIRO completar, descartando o outro.
    tokio::select! {
        r = reconnect_loop(&cli, proto, &tunnel) => r,
        _ = tokio::signal::ctrl_c() => { info!("encerrando"); Ok(()) }
    }
}

/// Reconecta para sempre com *exponential backoff*: espera 1s, 2s, 4s... até
/// 30s entre tentativas. Sem isso, um servidor fora do ar seria martelado em
/// loop apertado (e vários clientes juntos fariam um "thundering herd").
async fn reconnect_loop(cli: &Cli, proto: Proto, t: &Tunnel) -> Result<()> {
    let mut delay = Duration::from_secs(1);
    loop {
        let mut registered = false;
        match run_session(cli, proto, t, &mut registered).await {
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
        info!("reconectando em {delay:?}");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn run_session(cli: &Cli, proto: Proto, t: &Tunnel, registered: &mut bool) -> Result<()> {
    let mut ctl = TcpStream::connect(&cli.server)
        .await
        .with_context(|| format!("conectando em {}", cli.server))?;
    write_msg(&mut ctl, &Hello::Control).await?;

    let msg: ServerMsg = timeout(Duration::from_secs(5), read_msg(&mut ctl))
        .await
        .context("timeout")??;
    let ServerMsg::Challenge(nonce) = msg else {
        bail!("esperava Challenge")
    };
    write_msg(&mut ctl, &ClientMsg::Auth(auth_response(&cli.token, &nonce))).await?;

    match timeout(Duration::from_secs(5), read_msg(&mut ctl))
        .await
        .context("timeout")??
    {
        ServerMsg::AuthOk => {}
        ServerMsg::Error(e) => return Err(Fatal(e).into()),
        _ => bail!("resposta inesperada à autenticação"),
    }

    write_msg(
        &mut ctl,
        &ClientMsg::Register {
            proto,
            remote_port: t.remote_port,
        },
    )
    .await?;
    match timeout(Duration::from_secs(5), read_msg(&mut ctl))
        .await
        .context("timeout")??
    {
        ServerMsg::Registered { remote_port } => {
            info!("túnel {proto:?} ativo: <servidor>:{remote_port} -> {}", t.local);
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
                let (server, local) = (cli.server.clone(), t.local.clone());
                tokio::spawn(async move {
                    if let Err(e) = tcp_data(server, id, local).await {
                        debug!("conexão TCP: {e:#}");
                    }
                });
            }
            ServerMsg::NewUdpStream(id) => {
                let (server, local) = (cli.server.clone(), t.local.clone());
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
async fn tcp_data(server: String, id: ConnId, local: String) -> Result<()> {
    let mut data = TcpStream::connect(&server).await?;
    write_msg(&mut data, &Hello::Accept(id)).await?;
    // Conectamos no serviço local DEPOIS de aceitar no servidor: se o serviço
    // local estiver fora, o erro fecha `data` na hora e o usuário público é
    // desconectado rápido, em vez de ficar pendurado até o timeout.
    let mut svc = TcpStream::connect(&local)
        .await
        .with_context(|| format!("serviço local {local} indisponível"))?;
    let _ = (data.set_nodelay(true), svc.set_nodelay(true));
    copy_bidirectional(&mut data, &mut svc).await?;
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
