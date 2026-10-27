//! Sessão com o servidor: handshake, reconexão, heartbeat e despacho de streams.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use hbt_proto::mux::{self, Mode, MuxStream, StreamKind};
use hbt_proto::*;
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{interval, timeout};
use tracing::{debug, error, warn};

use crate::cli::{Conn, Plan};
use crate::forward::{tcp_conn, udp_data};

/// Erro que não adianta tentar de novo (ex.: token errado).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Fatal(pub String);

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
pub async fn reconnect_loop(conn: &Conn, plan: &Plan) -> Result<()> {
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
        .context("timeout")?
        // Um hbt-server antigo não entende o Hello novo e só fecha a conexão.
        .context("o servidor fechou a conexão no handshake (hbt-server antigo, sem multiplexação?)")?;
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

    // Daqui em diante a conexão é uma sessão multiplexada: o servidor abre
    // os streams e nós só os recebemos (o `Opener` daqui nunca é usado).
    let (in_tx, mut in_rx) = mpsc::channel::<mux::Stream>(mux::MAX_STREAMS);
    let (_opener, requests) = mux::opener();
    let mut session = AbortOnDrop(tokio::spawn(async move {
        if let Err(e) = mux::run(ctl, Mode::Client, requests, Some(in_tx)).await {
            debug!("sessão: {e}");
        }
    }));

    // Cada stream recebido ganha um task; o de controle volta para cá.
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<MuxStream>(1);
    let local = plan.local.clone();
    let _dispatcher = AbortOnDrop(tokio::spawn(async move {
        while let Some(raw) = in_rx.recv().await {
            tokio::spawn(dispatch(raw, local.clone(), ctl_tx.clone()));
        }
    }));

    // O servidor abre o stream de controle logo depois de `Registered`.
    let ctl = tokio::select! {
        c = timeout(Duration::from_secs(5), ctl_rx.recv()) => c
            .context("timeout esperando o stream de controle")?
            .context("sessão encerrada")?,
        _ = &mut session.0 => bail!("conexão com o servidor caiu"),
    };
    let (rd, mut wr) = tokio::io::split(ctl);
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

    // Se a sessão cair, a leitura do stream de controle falha e saímos daqui;
    // ao sair, os guards derrubam a sessão e todos os streams junto.
    loop {
        // Se o timeout estourar, `?` sai da função: a sessão é descartada
        // inteira, então perder bytes de uma leitura cancelada não importa.
        let msg: ServerMsg = timeout(HEARTBEAT_TIMEOUT, read_msg(&mut rd))
            .await
            .context("servidor sem heartbeat")??;
        match msg {
            ServerMsg::Heartbeat => {}
            ServerMsg::Error(e) => bail!("erro do servidor: {e}"),
            _ => bail!("mensagem inesperada"),
        }
    }
}

/// Um stream aberto pelo servidor: o primeiro byte diz o que ele carrega.
async fn dispatch(raw: mux::Stream, local: String, ctl: mpsc::Sender<MuxStream>) {
    let (kind, stream) = match MuxStream::accept(raw).await {
        Ok(x) => x,
        Err(e) => return debug!("stream inválido: {e}"),
    };
    match kind {
        StreamKind::Control => {
            let _ = ctl.send(stream).await;
        }
        StreamKind::Conn => {
            if let Err(e) = tcp_conn(stream, &local).await {
                warn!("conexão TCP: {e:#}");
            }
        }
        StreamKind::Udp => {
            if let Err(e) = udp_data(stream, &local).await {
                debug!("stream UDP: {e:#}");
            }
        }
    }
}
