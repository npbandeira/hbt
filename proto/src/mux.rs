//! Multiplexação: todo o tráfego de um túnel dentro de UMA conexão QUIC.
//!
//! O cliente abre a conexão e, no primeiro stream bidirecional, faz o
//! handshake (`Hello`, desafio HMAC, `Register`); esse mesmo stream segue como
//! stream de controle (heartbeats). Depois disso o servidor abre um *stream*
//! por usuário público (TCP/HTTP): cada um se comporta como uma conexão TCP
//! própria (ordem, controle de fluxo, fechamento por sentido) sem handshake e
//! sem que a perda de um pacote atrase os outros.
//!
//! O tráfego UDP não usa streams: cada datagrama público vira um *datagrama
//! QUIC* (RFC 9221), sem ordem nem retransmissão, como o UDP original.
//!
//! Primeiro byte de cada stream aberto pelo servidor: o [`StreamKind`]. O QUIC
//! só avisa o par de um stream novo quando chega o primeiro byte, e serviços
//! que falam primeiro (SSH, SMTP) precisam do stream visível antes de o
//! usuário público dizer qualquer coisa.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use quinn::{Connection, RecvStream, SendDatagramError, SendStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::UdpFrame;

/// Tipo de stream, mandado como primeiro byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamKind {
    /// Um usuário público (TCP ou HTTP): bytes crus, como um socket.
    Conn = 2,
}

impl StreamKind {
    fn from_byte(b: u8) -> io::Result<Self> {
        match b {
            2 => Ok(StreamKind::Conn),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tipo de stream desconhecido",
            )),
        }
    }
}

/// As duas metades de um stream bidirecional do QUIC com a interface de I/O
/// do Tokio, como se fosse um socket.
pub struct MuxStream {
    send: SendStream,
    recv: RecvStream,
}

impl MuxStream {
    /// Junta as duas metades de um stream.
    pub fn new(send: SendStream, recv: RecvStream) -> Self {
        MuxStream { send, recv }
    }

    /// Lê o byte de tipo de um stream recebido.
    pub async fn accept(send: SendStream, recv: RecvStream) -> io::Result<(StreamKind, MuxStream)> {
        let mut s = MuxStream::new(send, recv);
        let kind = StreamKind::from_byte(s.read_u8().await?)?;
        Ok((kind, s))
    }
}

impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }
    /// Fecha só o nosso sentido (FIN do QUIC), como `shutdown(Write)` em TCP.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

/// Abre streams na conexão do cliente. Barato de clonar.
#[derive(Clone)]
pub struct Opener {
    conn: Connection,
}

impl Opener {
    /// Opener da conexão `conn`.
    pub fn new(conn: Connection) -> Self {
        Opener { conn }
    }

    /// Abre um stream e manda o byte de tipo. Espera vaga se o cliente já tem
    /// [`crate::quic::MAX_STREAMS`] streams abertos.
    pub async fn open(&self, kind: StreamKind) -> io::Result<MuxStream> {
        let (send, recv) = self
            .conn
            .open_bi()
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
        let mut s = MuxStream::new(send, recv);
        s.write_u8(kind as u8).await?;
        s.flush().await?;
        Ok(s)
    }
}

/// Manda um datagrama UDP ao outro lado. Datagrama maior que o MTU do
/// caminho dá `TooLarge`; quem chama o descarta, como um roteador faria.
pub fn send_udp(conn: &Connection, frame: &UdpFrame) -> Result<(), SendDatagramError> {
    let body = bincode::serialize(frame).expect("UdpFrame sempre serializa");
    conn.send_datagram(body.into())
}

/// Espera o próximo datagrama UDP; ignora os que não decodificam.
pub async fn recv_udp(conn: &Connection) -> Result<UdpFrame, quinn::ConnectionError> {
    loop {
        let d = conn.read_datagram().await?;
        if let Ok(frame) = bincode::deserialize(&d) {
            return Ok(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic;

    /// Par de conexões QUIC reais em loopback: (servidor, cliente). Os
    /// `Endpoint`s vão junto para não serem descartados antes do teste acabar.
    struct Pair {
        server: Connection,
        client: Connection,
        _endpoints: (quinn::Endpoint, quinn::Endpoint),
    }

    async fn pair() -> Pair {
        let (cert, key) = quic::self_signed().unwrap();
        let fp = quic::fingerprint(&cert);
        let sep = quinn::Endpoint::server(
            quic::server_config(cert, key).unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let addr = sep.local_addr().unwrap();
        let mut cep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        cep.set_default_client_config(quic::client_config(&fp).unwrap());
        let connecting = cep.connect(addr, quic::SERVER_NAME).unwrap();
        let (client, server) = tokio::join!(connecting, async { sep.accept().await.unwrap().await });
        Pair {
            server: server.unwrap(),
            client: client.unwrap(),
            _endpoints: (sep, cep),
        }
    }

    /// O "cliente" ecoa cada stream aberto pelo "servidor".
    fn spawn_echo(client: Connection) {
        tokio::spawn(async move {
            while let Ok((send, recv)) = client.accept_bi().await {
                tokio::spawn(async move {
                    let (kind, mut s) = MuxStream::accept(send, recv).await.unwrap();
                    assert_eq!(kind, StreamKind::Conn);
                    let (mut r, mut w) = tokio::io::split(&mut s);
                    tokio::io::copy(&mut r, &mut w).await.unwrap();
                    s.shutdown().await.unwrap();
                });
            }
        });
    }

    #[tokio::test]
    async fn many_concurrent_streams_echo_independently() {
        let p = pair().await;
        spawn_echo(p.client.clone());
        let op = Opener::new(p.server.clone());
        // Mais streams do que o limite ao mesmo tempo: os excedentes esperam
        // vaga em vez de falhar.
        let mut tasks = Vec::new();
        for i in 0..(quic::MAX_STREAMS as usize + 200) {
            let op = op.clone();
            tasks.push(tokio::spawn(async move {
                let mut s = op.open(StreamKind::Conn).await.unwrap();
                let msg = format!("stream {i}");
                s.write_all(msg.as_bytes()).await.unwrap();
                s.shutdown().await.unwrap();
                let mut got = String::new();
                s.read_to_string(&mut got).await.unwrap();
                assert_eq!(got, msg);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    }

    #[tokio::test]
    async fn large_transfer_survives_flow_control() {
        let p = pair().await;
        spawn_echo(p.client.clone());
        let mut s = Opener::new(p.server.clone())
            .open(StreamKind::Conn)
            .await
            .unwrap();
        let data: Vec<u8> = (0..5_000_000u32).map(|i| i as u8).collect();
        let expected = data.clone();
        let (mut r, mut w) = tokio::io::split(&mut s);
        let writer = async {
            w.write_all(&data).await.unwrap();
            w.shutdown().await.unwrap();
        };
        let reader = async {
            let mut got = Vec::new();
            r.read_to_end(&mut got).await.unwrap();
            got
        };
        let ((), got) = tokio::join!(writer, reader);
        assert_eq!(got.len(), expected.len());
        assert!(got == expected);
    }

    #[tokio::test]
    async fn udp_datagrams_roundtrip_in_both_directions() {
        let p = pair().await;
        let sent = UdpFrame {
            peer: "1.2.3.4:5".parse().unwrap(),
            data: vec![9; 1000],
        };
        send_udp(&p.server, &sent).unwrap();
        let got = recv_udp(&p.client).await.unwrap();
        assert_eq!((got.peer, got.data.as_slice()), (sent.peer, sent.data.as_slice()));
        send_udp(&p.client, &sent).unwrap();
        assert_eq!(recv_udp(&p.server).await.unwrap().data, sent.data);
    }

    #[tokio::test]
    async fn oversized_datagram_is_refused_not_fragmented() {
        let p = pair().await;
        let big = UdpFrame {
            peer: "1.2.3.4:5".parse().unwrap(),
            data: vec![0; 60_000],
        };
        assert!(matches!(
            send_udp(&p.server, &big),
            Err(SendDatagramError::TooLarge)
        ));
    }

    #[tokio::test]
    async fn wrong_fingerprint_is_rejected() {
        let (cert, key) = quic::self_signed().unwrap();
        let sep = quinn::Endpoint::server(
            quic::server_config(cert, key).unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let addr = sep.local_addr().unwrap();
        let mut cep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        cep.set_default_client_config(quic::client_config(&"00".repeat(32)).unwrap());
        let (client, _) = tokio::join!(cep.connect(addr, quic::SERVER_NAME).unwrap(), async {
            // Só para o servidor responder ao handshake; o resultado não importa.
            if let Some(i) = sep.accept().await {
                let _ = i.await;
            }
        });
        assert!(client.is_err());
    }
}
