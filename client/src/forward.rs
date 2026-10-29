//! Repasse dos streams recebidos do servidor para o serviço local (TCP e UDP).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use hbt_proto::mux::{recv_udp, send_udp, MuxStream};
use hbt_proto::*;
use quinn::{Connection, SendDatagramError};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;
use tracing::debug;

/// Sem tráfego do lado local por este tempo, a "sessão" UDP é descartada.
const UDP_IDLE: Duration = Duration::from_secs(60);

/// Uma conexão de usuário: cola o stream no serviço local.
/// Erro ao conectar no serviço sobe (vira `warn`); erros da cópia depois disso
/// são o usuário/serviço fechando e ficam em `debug`.
pub async fn tcp_conn(mut stream: MuxStream, local: &str) -> Result<()> {
    // Se o serviço local estiver fora, o `?` descarta o stream: o servidor
    // recebe um reset e o usuário público é desconectado na hora.
    let mut svc = TcpStream::connect(local)
        .await
        .with_context(|| format!("serviço local {local} indisponível"))?;
    let _ = svc.set_nodelay(true);
    if let Err(e) = copy_bidirectional(&mut stream, &mut svc).await {
        debug!("conexão TCP: {e}");
    }
    Ok(())
}

type Sessions = Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>>;

/// Túnel UDP. Cada "peer" público ganha um socket UDP local próprio, para o
/// serviço local enxergar origens distintas e as respostas voltarem ao peer certo.
/// Os datagramas chegam e saem como datagramas QUIC (sem ordem nem retransmissão).
pub async fn udp_data(conn: Connection, local: &str) -> Result<()> {
    let local_addr = tokio::net::lookup_host(local)
        .await?
        .next()
        .with_context(|| format!("não resolvi {local}"))?;

    let sessions: Sessions = Arc::default();
    loop {
        let frame = recv_udp(&conn).await?;

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
                    conn.clone(),
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

async fn local_to_peer(sock: Arc<UdpSocket>, peer: SocketAddr, conn: Connection, sessions: Sessions) {
    let mut buf = vec![0u8; 65535];
    // `while let` com padrão que só casa em sucesso: erro de I/O ou ociosidade
    // (Elapsed) encerram o loop e liberam o socket.
    while let Ok(Ok(n)) = timeout(UDP_IDLE, sock.recv(&mut buf)).await {
        let frame = UdpFrame {
            peer,
            data: buf[..n].to_vec(),
        };
        match send_udp(&conn, &frame) {
            Ok(()) => {}
            Err(SendDatagramError::ConnectionLost(_)) => break,
            // Maior que o MTU do caminho: descartado, como um roteador faria.
            Err(e) => debug!("resposta a {peer} ({n}B) descartada: {e}"),
        }
    }
    sessions.lock().unwrap().remove(&peer);
}
