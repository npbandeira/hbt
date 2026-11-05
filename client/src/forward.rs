//! Repasse dos streams recebidos do servidor para o serviço local (TCP e UDP).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use hbt_proto::mux::{recv_udp, send_udp, MuxStream};
use hbt_proto::*;
use quinn::{Connection, SendDatagramError};
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::{timeout, Instant};
use tracing::debug;

use crate::stats;

/// Sem tráfego do lado local por este tempo, a "sessão" UDP é descartada.
const UDP_IDLE: Duration = Duration::from_secs(60);

/// Uma conexão de usuário: cola o stream no serviço local.
/// Erro ao conectar no serviço sobe (vira `warn`); erros da cópia depois disso
/// são o usuário/serviço fechando e ficam em `debug`.
/// Com `http`, a primeira requisição da conexão é logada (`GET /x 200 12ms`).
pub async fn tcp_conn(mut stream: MuxStream, local: &str, http: bool) -> Result<()> {
    let _guard = stats::conn_guard();
    // Se o serviço local estiver fora, o `?` descarta o stream: o servidor
    // recebe um reset e o usuário público é desconectado na hora.
    let mut svc = TcpStream::connect(local)
        .await
        .with_context(|| format!("serviço local {local} indisponível"))?;
    let _ = svc.set_nodelay(true);
    let res = if http {
        http_copy(&mut stream, &mut svc).await
    } else {
        copy_bidirectional(&mut stream, &mut svc).await
    };
    match res {
        Ok((inn, out)) => stats::add_bytes(inn, out),
        Err(e) => debug!("conexão TCP: {e}"),
    }
    Ok(())
}

const HEAD: usize = 8192;

/// Como `copy_bidirectional`, mas espia o primeiro pedaço de cada sentido para logar.
/// shortcut: só a 1ª requisição de cada conexão keep-alive é logada; parser por requisição se isso incomodar.
async fn http_copy(stream: &mut MuxStream, svc: &mut TcpStream) -> std::io::Result<(u64, u64)> {
    let mut head = vec![0u8; HEAD];
    let n = stream.read(&mut head).await?;
    if n == 0 {
        return Ok((0, 0));
    }
    let req = parse_request_line(&head[..n]);
    svc.write_all(&head[..n]).await?;
    let start = Instant::now();

    let (mut sr, mut sw) = tokio::io::split(stream);
    let (mut vr, mut vw) = svc.split();
    // Os dois sentidos andam juntos: esperar a resposta antes de repassar o
    // resto do corpo da requisição travaria uploads grandes.
    let up = async {
        let c = tokio::io::copy(&mut sr, &mut vw).await?;
        let _ = vw.shutdown().await;
        Ok::<_, std::io::Error>(c + n as u64)
    };
    let down = async {
        let m = vr.read(&mut head).await?;
        if let (Some((method, path)), Some(code)) = (&req, parse_status(&head[..m])) {
            stats::log(&format!(
                "  {:<7} {path}  {}  {}",
                stats::paint(stats::BOLD, method),
                stats::paint(stats::status_color(code), &code.to_string()),
                stats::paint(stats::DIM, &format!("{}ms", start.elapsed().as_millis())),
            ));
        }
        sw.write_all(&head[..m]).await?;
        let c = tokio::io::copy(&mut vr, &mut sw).await?;
        let _ = sw.shutdown().await;
        Ok::<_, std::io::Error>(c + m as u64)
    };
    tokio::try_join!(up, down)
}

/// `GET /x HTTP/1.1` => `("GET", "/x")`. `None` se não parece HTTP ou tem caractere
/// de controle (o caminho vem de um usuário público e vai para o nosso terminal).
fn parse_request_line(buf: &[u8]) -> Option<(String, String)> {
    let line = buf.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    if line.chars().any(char::is_control) {
        return None;
    }
    let mut it = line.split(' ');
    let (method, path, version) = (it.next()?, it.next()?, it.next()?);
    let ok = !method.is_empty()
        && method.bytes().all(|b| b.is_ascii_uppercase())
        && version.starts_with("HTTP/")
        && it.next().is_none();
    ok.then(|| (method.to_owned(), path.to_owned()))
}

/// `HTTP/1.1 200 OK` => `200`.
fn parse_status(buf: &[u8]) -> Option<u16> {
    let rest = buf.strip_prefix(b"HTTP/1.")?;
    let code = rest.get(2..5)?;
    code.iter()
        .all(u8::is_ascii_digit)
        .then(|| std::str::from_utf8(code).ok()?.parse().ok())?
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
        stats::add_bytes(frame.data.len() as u64, 0);
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
        stats::add_bytes(0, n as u64);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_line() {
        assert_eq!(
            parse_request_line(b"GET /a?b=1 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("GET".into(), "/a?b=1".into()))
        );
        assert_eq!(
            parse_request_line(b"GET /a HTTP/1.1"),
            Some(("GET".into(), "/a".into()))
        );
    }

    #[test]
    fn request_line_rejects_non_http() {
        assert_eq!(parse_request_line(b"\x16\x03\x01binary"), None);
        assert_eq!(parse_request_line(b"GET /a"), None);
        assert_eq!(parse_request_line(b"GET /\x1b[2J HTTP/1.1\r\n"), None);
        assert_eq!(parse_request_line(b""), None);
    }

    #[test]
    fn status() {
        assert_eq!(parse_status(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_status(b"HTTP/1.0 404"), Some(404));
        assert_eq!(parse_status(b"HTTP/1.1 2"), None);
        assert_eq!(parse_status(b"SSH-2.0"), None);
    }
}
