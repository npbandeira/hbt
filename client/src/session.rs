//! Sessão com o servidor: conexão QUIC, handshake, reconexão, heartbeat e despacho de streams.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use hbt_proto::mux::{MuxStream, StreamKind};
use hbt_proto::*;
use quinn::{Connection, Endpoint};
use tokio::io::BufReader;
use tokio::net::lookup_host;
use tokio::time::{interval, timeout};
use tracing::{debug, error, warn};

use crate::cli::{Conn, Plan};
use crate::forward::{tcp_conn, udp_data};
use crate::stats::{self, State};

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
        stats::set_state(State::Reconnecting);
        warn!("reconectando em {delay:?}");
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// Abre a conexão QUIC, roda a sessão e fecha a conexão ao final (em
/// qualquer caminho de saída), para o servidor perceber na hora.
async fn run_session(conn: &Conn, plan: &Plan, registered: &mut bool) -> Result<()> {
    let addr = lookup_host(&conn.server)
        .await
        .ok()
        .and_then(|mut a| a.next())
        .with_context(|| format!("não resolvi {}", conn.server))?;
    let bind = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let endpoint = Endpoint::client(bind.parse()?)?;
    let cfg = quic::client_config(&conn.fingerprint)?;
    let quic_conn = timeout(
        Duration::from_secs(10),
        endpoint.connect_with(cfg, addr, quic::SERVER_NAME)?,
    )
    .await
    .context("timeout conectando (a porta de controle é UDP: o firewall libera?)")?
    .with_context(|| {
        format!(
            "conectando em {} (confira o servidor, a porta UDP e o fingerprint)",
            conn.server
        )
    })?;

    let result = serve_session(&quic_conn, conn, plan, registered).await;
    quic_conn.close(0u32.into(), b"bye");
    // Dá tempo de o CONNECTION_CLOSE sair antes de largar o socket.
    let _ = timeout(Duration::from_millis(500), endpoint.wait_idle()).await;
    result
}

async fn serve_session(
    quic_conn: &Connection,
    conn: &Conn,
    plan: &Plan,
    registered: &mut bool,
) -> Result<()> {
    // O primeiro stream faz o handshake e depois segue como stream de controle.
    let (send, recv) = quic_conn.open_bi().await?;
    let mut ctl = MuxStream::new(send, recv);
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
            stats::set_state(State::Online);
            *registered = true;
        }
        // Pode ser transitório (servidor ainda não notou que a sessão antiga
        // morreu e a porta segue ocupada), então NÃO é Fatal: tentamos de novo.
        ServerMsg::Error(e) => bail!("servidor recusou: {e}"),
        _ => bail!("resposta inesperada ao registro"),
    }

    // Daqui em diante o servidor abre um stream por usuário público (TCP/HTTP)
    // ou manda datagramas (UDP); nós só recebemos.
    let _dispatcher = AbortOnDrop(tokio::spawn({
        let (quic_conn, local) = (quic_conn.clone(), plan.local.clone());
        let http = matches!(plan.spec, TunnelSpec::Http { .. });
        async move {
            while let Ok((send, recv)) = quic_conn.accept_bi().await {
                tokio::spawn(dispatch(send, recv, local.clone(), http));
            }
        }
    }));
    let _udp = matches!(plan.spec, TunnelSpec::Udp { .. }).then(|| {
        let (quic_conn, local) = (quic_conn.clone(), plan.local.clone());
        AbortOnDrop(tokio::spawn(async move {
            if let Err(e) = udp_data(quic_conn, &local).await {
                debug!("túnel UDP: {e:#}");
            }
        }))
    });

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

    // Se a conexão cair, a leitura do stream de controle falha e saímos daqui;
    // ao sair, os guards derrubam os tasks e o chamador fecha a conexão.
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

/// Um stream aberto pelo servidor para um usuário público.
async fn dispatch(send: quinn::SendStream, recv: quinn::RecvStream, local: String, http: bool) {
    let (kind, stream) = match MuxStream::accept(send, recv).await {
        Ok(x) => x,
        Err(e) => return debug!("stream inválido: {e}"),
    };
    match kind {
        StreamKind::Conn => {
            if let Err(e) = tcp_conn(stream, &local, http).await {
                warn!("conexão TCP: {e:#}");
            }
        }
    }
}
