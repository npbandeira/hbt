//! Vida de um túnel: heartbeat, revogação e repasse de TCP/UDP.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use hbt_proto::mux::{recv_udp, send_udp, MuxStream, Opener, StreamKind};
use hbt_proto::*;
use quinn::{Connection, SendDatagramError};
use tokio::io::{copy_bidirectional, BufReader};
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::{interval, timeout};
use tracing::{debug, info, warn};

use crate::control::Bound;
use crate::metrics::{ConnError, Kind, Metrics};
use crate::state::State;
use crate::tokens::TokenRecord;

/// Vive enquanto o cliente estiver conectado.
///
/// `ctl` é o stream do handshake, que segue como stream de controle:
/// heartbeats nos dois sentidos. Todo o resto da conexão (`conn`) é aberto
/// por demanda: streams (via `opener`) e datagramas.
pub async fn run_tunnel(
    ctl: MuxStream,
    conn: Connection,
    state: Arc<State>,
    bound: Bound,
    record: TokenRecord,
    opener: Opener,
) -> Result<()> {
    let (rd, mut wr) = tokio::io::split(ctl);
    let mut rd = BufReader::new(rd);

    // Leitura do stream de controle vive em task próprio porque `read_msg`
    // não é cancel-safe (veja proto/src/lib.rs): não pode concorrer num
    // `select!`. Esperar um JoinHandle, por outro lado, é seguro.
    let mut reader = AbortOnDrop(tokio::spawn(async move {
        loop {
            match timeout(HEARTBEAT_TIMEOUT, read_msg::<_, ClientMsg>(&mut rd)).await {
                Ok(Ok(_heartbeat)) => {}
                Ok(Err(e)) => return debug!("controle fechou: {e}"),
                Err(_) => return debug!("cliente sem heartbeat; considerado morto"),
            }
        }
    }));

    let _session = state.metrics.session(match bound {
        Bound::Tcp(_) => Kind::Tcp,
        Bound::Udp(_) => Kind::Udp,
        Bound::Http(_) => Kind::Http,
    });

    // Túneis TCP/UDP têm um task; HTTP só uma rota (guard) que vive até o fim.
    let mut tunnel = None;
    let mut _route = None; // com nome: o guard só cai quando a função termina
    match bound {
        Bound::Tcp(l) => {
            tunnel = Some(AbortOnDrop(tokio::spawn(tcp_accept_loop(
                l,
                state.clone(),
                opener,
            ))))
        }
        Bound::Udp(s) => {
            tunnel = Some(AbortOnDrop(tokio::spawn(udp_tunnel(
                s,
                state.clone(),
                conn.clone(),
            ))))
        }
        Bound::Http(guard) => _route = Some(guard),
    }

    let mut beat = interval(HEARTBEAT_EVERY);
    loop {
        tokio::select! {
            _ = beat.tick() => {
                // Revogação: se o token sumiu do arquivo, derruba a sessão.
                // Erro de leitura NÃO derruba (melhor manter túneis de pé
                // do que cortar todo mundo por um problema momentâneo).
                if matches!(state.tokens.find(&record.id).await, Ok(None)) {
                    warn!("token '{}' revogado: encerrando sessão", record.name);
                    break;
                }
                write_msg(&mut wr, &ServerMsg::Heartbeat).await?
            }
            _ = &mut reader.0 => break,
            _ = tunnel_done(&mut tunnel) => break,
            _ = conn.closed() => break,
        }
    }
    info!("túnel de '{}' encerrado", record.name);
    // Ao sair, `reader` e `tunnel` sofrem Drop => tasks abortados => o
    // listener é fechado e a porta pública liberada. O close derruba a
    // conexão com o cliente (e todos os streams). Isso é RAII.
    conn.close(0u32.into(), b"tunnel closed");
    Ok(())
}

/// Completa quando o task do túnel termina; para HTTP (sem task) nunca completa.
async fn tunnel_done(tunnel: &mut Option<AbortOnDrop>) {
    match tunnel {
        Some(t) => {
            let _ = (&mut t.0).await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Abre um stream até o cliente para um usuário público. Dá 10s: com o túnel
/// no limite de conexões simultâneas, a abertura espera vaga.
pub async fn open_conn(opener: &Opener, metrics: &Metrics) -> Result<MuxStream, ConnError> {
    let asked = Instant::now();
    let err = match timeout(Duration::from_secs(10), opener.open(StreamKind::Conn)).await {
        Ok(Ok(s)) => {
            metrics.data_wait(asked.elapsed());
            return Ok(s);
        }
        Ok(Err(_)) => ConnError::TunnelOffline,
        Err(_) => ConnError::DataTimeout,
    };
    metrics.conn_error(err);
    Err(err)
}

async fn tcp_accept_loop(listener: TcpListener, state: Arc<State>, opener: Opener) {
    loop {
        let (mut public, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("accept público falhou: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = public.set_nodelay(true);
        let (state, opener) = (state.clone(), opener.clone());
        tokio::spawn(async move {
            let _conn = state.metrics.conn(Kind::Tcp);
            match open_conn(&opener, &state.metrics).await {
                Ok(mut data) => {
                    // Copia nos dois sentidos até um lado fechar.
                    match copy_bidirectional(&mut public, &mut data).await {
                        Ok((up, down)) => {
                            state.metrics.bytes(Kind::Tcp, up, down);
                            debug!("{peer}: fechou ({up}B ↑, {down}B ↓)")
                        }
                        Err(e) => debug!("{peer}: {e}"),
                    }
                }
                Err(e) => debug!("{peer}: sem stream até o cliente ({e:?})"),
            }
        });
    }
}

/// UDP: cada datagrama público vira um datagrama QUIC (sem ordem nem
/// retransmissão) e vice-versa.
async fn udp_tunnel(socket: UdpSocket, state: Arc<State>, conn: Connection) {
    let socket = Arc::new(socket);

    // Dois sentidos concorrentes no mesmo task: o primeiro que terminar
    // (erro/fechamento) encerra o outro, porque o `select!` descarta o perdedor.
    tokio::select! {
        r = udp_to_client(socket.clone(), &conn, &state.metrics) => debug!("udp->cliente: {r:?}"),
        r = client_to_udp(socket, &conn, &state.metrics) => debug!("cliente->udp: {r:?}"),
    }
}

async fn udp_to_client(socket: Arc<UdpSocket>, conn: &Connection, metrics: &Metrics) -> io::Result<()> {
    let mut buf = vec![0u8; 65535];
    loop {
        let (n, peer) = socket.recv_from(&mut buf).await?;
        metrics.udp_in(n);
        let frame = UdpFrame {
            peer,
            data: buf[..n].to_vec(),
        };
        match send_udp(conn, &frame) {
            Ok(()) => {}
            Err(SendDatagramError::ConnectionLost(e)) => return Err(io::Error::other(e)),
            // Maior que o MTU do caminho: descartado, como um roteador faria.
            Err(e) => debug!("datagrama de {peer} ({n}B) descartado: {e}"),
        }
    }
}

async fn client_to_udp(socket: Arc<UdpSocket>, conn: &Connection, metrics: &Metrics) -> io::Result<()> {
    loop {
        let f = recv_udp(conn).await.map_err(io::Error::other)?;
        socket.send_to(&f.data, f.peer).await?;
        metrics.udp_out(f.data.len());
    }
}
