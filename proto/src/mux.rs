//! Multiplexação: todas as conexões de um túnel dentro de UMA conexão TCP.
//!
//! Antes, cada usuário público fazia o cliente abrir uma conexão TCP nova até
//! o servidor: ~2 RTT extras por conexão e milhares de conexões curtas saindo
//! da máquina do cliente (portas efêmeras, NAT). Agora, depois do handshake,
//! a conexão de controle vira uma sessão [yamux]: o servidor abre um *stream*
//! lógico por usuário, e cada stream se comporta como uma conexão TCP própria
//! (ordem, controle de fluxo, fechamento por sentido), mas sem handshake.
//!
//! O yamux exige que UM task fique "girando" a conexão (`poll_next_inbound`)
//! para ler e escrever os frames de todos os streams. Esse task é [`run`];
//! os outros pedem streams novos a ele via [`Opener`].
//!
//! Primeiro byte de cada stream: o [`StreamKind`], escrito por quem abre.
//!
//! [yamux]: https://github.com/hashicorp/yamux/blob/master/spec.md

use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

pub use yamux::{Mode, Stream};

/// Conexões simultâneas por túnel. Acima disso, novas conexões esperam vaga.
pub const MAX_STREAMS: usize = 1024;

/// O yamux DERRUBA a sessão inteira se abrirmos mais streams que o limite
/// dele, e só esquece um stream fechado algum tempo depois. Por isso o limite
/// dele é o dobro do nosso: a fila do nosso semáforo nunca o alcança.
const YAMUX_MAX_STREAMS: usize = 2 * MAX_STREAMS;

/// Tipo de stream, mandado como primeiro byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamKind {
    /// Heartbeats e erros (`ServerMsg`/`ClientMsg`), um por sessão.
    Control = 1,
    /// Um usuário público (TCP ou HTTP): bytes crus, como um socket.
    Conn = 2,
    /// Todos os datagramas de um túnel UDP, em `UdpFrame`s.
    Udp = 3,
}

impl StreamKind {
    fn from_byte(b: u8) -> io::Result<Self> {
        Ok(match b {
            1 => StreamKind::Control,
            2 => StreamKind::Conn,
            3 => StreamKind::Udp,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tipo de stream desconhecido",
                ))
            }
        })
    }
}

/// Um stream do yamux com a interface de I/O do Tokio (o yamux usa os traits
/// do crate `futures`; `Compat` faz a ponte). Segura uma vaga do semáforo
/// enquanto viver.
pub struct MuxStream {
    inner: Compat<yamux::Stream>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl MuxStream {
    /// Lê o byte de tipo de um stream recebido.
    pub async fn accept(stream: yamux::Stream) -> io::Result<(StreamKind, MuxStream)> {
        let mut s = MuxStream {
            inner: stream.compat(),
            _permit: None,
        };
        let kind = StreamKind::from_byte(s.read_u8().await?)?;
        Ok((kind, s))
    }
}

// Delegação simples: `Compat<yamux::Stream>` é `Unpin`, então dá para
// reempacotar o `Pin` sem `unsafe`.
impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    /// Fecha só o nosso sentido (FIN do yamux), como `shutdown(Write)` em TCP.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

type OpenReply = oneshot::Sender<yamux::Result<yamux::Stream>>;

/// Pede streams novos ao task que gira a sessão. Barato de clonar.
#[derive(Clone)]
pub struct Opener {
    tx: mpsc::Sender<OpenReply>,
    slots: Arc<Semaphore>,
}

/// Pedidos de abertura ainda não entregues a [`run`].
pub struct Requests(mpsc::Receiver<OpenReply>);

/// Cria o par opener/pedidos ANTES de a sessão existir: o servidor registra o
/// túnel (e a rota HTTP) durante o handshake, quando ainda fala o protocolo
/// sem multiplexação. Pedidos feitos antes de [`run`] começar só esperam.
pub fn opener() -> (Opener, Requests) {
    let (tx, rx) = mpsc::channel(64);
    let slots = Arc::new(Semaphore::new(MAX_STREAMS));
    (Opener { tx, slots }, Requests(rx))
}

impl Opener {
    /// Abre um stream e manda o byte de tipo. Espera vaga se o túnel já tem
    /// `MAX_STREAMS` streams abertos.
    pub async fn open(&self, kind: StreamKind) -> io::Result<MuxStream> {
        let closed = || io::Error::new(io::ErrorKind::BrokenPipe, "sessão do túnel encerrada");
        let permit = self.slots.clone().acquire_owned().await.map_err(|_| closed())?;
        let (reply, rx) = oneshot::channel();
        self.tx.send(reply).await.map_err(|_| closed())?;
        let stream = rx.await.map_err(|_| closed())?.map_err(io::Error::other)?;
        let mut s = MuxStream {
            inner: stream.compat(),
            _permit: Some(permit),
        };
        s.write_u8(kind as u8).await?;
        s.flush().await?;
        Ok(s)
    }
}

/// Janela inicial de cada stream no yamux.
const STREAM_WINDOW: usize = 256 * 1024;

/// Folga, dividida entre os streams, para as janelas crescerem além de
/// `STREAM_WINDOW`. O yamux dobra a janela de um stream rápido até chegar ao
/// produto banda×atraso, mas só dentro dessa folga. Sem ela, todo stream fica
/// preso em 256 KiB por RTT: com 63 ms de ping, ~4 MB/s, o que derrubava o
/// túnel UDP (um stream só) para ~4 mil pacotes/s. É um teto de memória, não
/// uma reserva: só é usado se o leitor de um stream ficar para trás.
const WINDOW_HEADROOM: usize = 64 * 1024 * 1024;

fn config() -> yamux::Config {
    let mut cfg = yamux::Config::default();
    // A janela total precisa caber 256 KiB por stream (o yamux confere com
    // `assert!`), por isso ela é ajustada ANTES do número de streams.
    cfg.set_max_connection_receive_window(Some(YAMUX_MAX_STREAMS * STREAM_WINDOW + WINDOW_HEADROOM));
    cfg.set_max_num_streams(YAMUX_MAX_STREAMS);
    cfg
}

/// Gira a sessão até a conexão cair. Atende os pedidos de `requests` e
/// entrega streams abertos pelo outro lado em `inbound` (se `None`, eles são
/// recusados: o servidor não aceita streams do cliente).
///
/// Termina com `Ok` quando o outro lado fecha a sessão normalmente.
pub async fn run(
    socket: TcpStream,
    mode: Mode,
    requests: Requests,
    inbound: Option<mpsc::Sender<yamux::Stream>>,
) -> io::Result<()> {
    let _ = socket.set_nodelay(true);
    // O yamux escreve cada frame em DUAS chamadas (cabeçalho de 12 bytes e
    // corpo) e lê também em duas. Direto no socket, com TCP_NODELAY, isso
    // vira dois pacotes e duas syscalls por frame. O buffer junta cabeçalho e
    // corpo num write só (o yamux dá flush entre frames) e lê vários frames
    // por syscall.
    let socket = BufStream::with_capacity(64 * 1024, 64 * 1024, socket);
    let mut conn = yamux::Connection::new(socket.compat(), config(), mode);
    let mut requests = Some(requests.0);
    let mut waiting: Option<OpenReply> = None;

    poll_fn(|cx| {
        // 1) Pedidos de abertura: um por vez. `poll_new_outbound` pode ficar
        //    pendente (muitos streams esperando ACK do outro lado); nesse caso
        //    o pedido espera em `waiting` e o yamux nos acorda depois.
        loop {
            if waiting.is_none() {
                match requests.as_mut().map(|r| r.poll_recv(cx)) {
                    Some(Poll::Ready(Some(reply))) => waiting = Some(reply),
                    // Todos os `Opener` sumiram: só paramos de atender.
                    Some(Poll::Ready(None)) => requests = None,
                    Some(Poll::Pending) | None => {}
                }
            }
            let Some(reply) = waiting.take() else { break };
            match conn.poll_new_outbound(cx) {
                Poll::Ready(r) => {
                    let failed = r.is_err();
                    let _ = reply.send(r); // quem pediu pode ter desistido
                    if failed {
                        break; // a sessão está morrendo; o passo 2 vai notar
                    }
                }
                Poll::Pending => {
                    waiting = Some(reply);
                    break;
                }
            }
        }

        // 2) Faz a sessão andar: lê frames, escreve o que os streams
        //    produziram e recebe streams abertos pelo outro lado.
        loop {
            match conn.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(stream))) => {
                    // Fila cheia ou ninguém ouvindo: o drop reseta o stream.
                    if let Some(tx) = &inbound {
                        let _ = tx.try_send(stream);
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(io::Error::other(e))),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::time::Instant;

    /// Sessão real sobre TCP local: o "servidor" abre streams, o "cliente" ecoa.
    async fn pair() -> (Opener, tokio::task::JoinHandle<io::Result<()>>) {
        pair_with_delay(None).await
    }

    /// Como `pair`, mas com um proxy no meio que atrasa cada sentido em
    /// `one_way`: simula a latência da internet em localhost.
    async fn pair_with_delay(one_way: Option<Duration>) -> (Opener, tokio::task::JoinHandle<io::Result<()>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut addr = l.local_addr().unwrap();
        if let Some(d) = one_way {
            addr = delay_proxy(addr, d).await;
        }
        let client = tokio::spawn(async move {
            let s = TcpStream::connect(addr).await.unwrap();
            let (in_tx, mut in_rx) = mpsc::channel(1024);
            let (_o, reqs) = opener();
            tokio::spawn(async move {
                while let Some(stream) = in_rx.recv().await {
                    tokio::spawn(async move {
                        let (kind, mut s) = MuxStream::accept(stream).await.unwrap();
                        assert_eq!(kind, StreamKind::Conn);
                        let (mut r, mut w) = tokio::io::split(&mut s);
                        tokio::io::copy(&mut r, &mut w).await.unwrap();
                        s.shutdown().await.unwrap();
                    });
                }
            });
            run(s, Mode::Client, reqs, Some(in_tx)).await
        });
        let (sock, _) = l.accept().await.unwrap();
        let (op, reqs) = opener();
        let session = tokio::spawn(run(sock, Mode::Server, reqs, None));
        drop(client);
        (op, session)
    }

    /// Proxy TCP que entrega cada pedaço `one_way` depois de recebê-lo, sem
    /// limite de banda: um "cano longo" como a internet.
    async fn delay_proxy(target: std::net::SocketAddr, one_way: Duration) -> std::net::SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (a, _) = l.accept().await.unwrap();
            let b = TcpStream::connect(target).await.unwrap();
            let (ar, aw) = a.into_split();
            let (br, bw) = b.into_split();
            tokio::spawn(delay_pipe(ar, bw, one_way));
            tokio::spawn(delay_pipe(br, aw, one_way));
        });
        addr
    }

    async fn delay_pipe(
        mut r: impl AsyncRead + Unpin,
        mut w: impl AsyncWrite + Unpin + Send + 'static,
        d: Duration,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
        tokio::spawn(async move {
            while let Some((at, chunk)) = rx.recv().await {
                tokio::time::sleep_until(at).await;
                if w.write_all(&chunk).await.is_err() {
                    return;
                }
            }
            let _ = w.shutdown().await;
        });
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n @ 1..) = r.read(&mut buf).await {
            let _ = tx.send((Instant::now() + d, buf[..n].to_vec()));
        }
    }

    /// Regressão: com a janela presa em 256 KiB, um stream não passa de
    /// ~256 KiB por RTT (aqui, 40 ms => ~6 MB/s em cada sentido). Com folga
    /// para o auto-ajuste do yamux, a janela cresce e o stream acompanha o cano.
    #[tokio::test]
    async fn stream_window_grows_on_high_latency_links() {
        let (op, _session) = pair_with_delay(Some(Duration::from_millis(20))).await;
        let mut s = op.open(StreamKind::Conn).await.unwrap();
        let data = vec![7u8; 32 * 1024 * 1024];
        let started = Instant::now();
        let (mut r, mut w) = tokio::io::split(&mut s);
        let writer = async {
            w.write_all(&data).await.unwrap();
            w.shutdown().await.unwrap();
        };
        let reader = async {
            let mut got = 0usize;
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = r.read(&mut buf).await {
                got += n;
            }
            got
        };
        let ((), got) = tokio::join!(writer, reader);
        let secs = started.elapsed().as_secs_f64();
        assert_eq!(got, data.len());
        let mb_s = got as f64 / secs / 1e6;
        assert!(mb_s > 12.0, "só {mb_s:.1} MB/s: a janela do stream não cresceu");
    }

    #[tokio::test]
    async fn many_concurrent_streams_echo_independently() {
        let (op, _session) = pair().await;
        // Mais streams do que MAX_STREAMS ao mesmo tempo: os excedentes
        // esperam vaga em vez de derrubar a sessão.
        let mut tasks = Vec::new();
        for i in 0..(MAX_STREAMS + 200) {
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
        let (op, _session) = pair().await;
        let mut s = op.open(StreamKind::Conn).await.unwrap();
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
}
