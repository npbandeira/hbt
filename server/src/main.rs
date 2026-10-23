//! hbt-server: roda na nuvem (IP público). Aceita clientes na porta de
//! controle e abre portas públicas que redirecionam o tráfego pelo túnel.
//!
//! Fluxo TCP (resumo; detalhes em docs/GUIDE.md):
//!
//!   usuário --> [porta pública no servidor]
//!                   |  (1) servidor manda NewConn(id) pelo canal de controle
//!                   v
//!               cliente abre NOVA conexão ao servidor: Hello::Accept(id)   (2)
//!                   |  (3) servidor cola (copy) usuário <-> essa conexão
//!                   v
//!               cliente conecta no serviço local e cola as duas pontas

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use hbt_proto::*;
use tokio::io::{copy_bidirectional, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, timeout};
use tracing::{debug, info, warn};

#[derive(Parser)]
#[command(about = "hbt reverse tunnel server")]
struct Args {
    /// Endereço da porta de controle (onde os clientes se conectam).
    #[arg(long, default_value = "0.0.0.0:7000")]
    control_bind: String,

    /// IP em que as portas públicas dos túneis são abertas.
    #[arg(long, default_value = "0.0.0.0")]
    tunnel_ip: IpAddr,

    /// Faixa de portas que clientes podem pedir, ex: 20000-30000.
    #[arg(long, default_value = "20000-30000", value_parser = parse_range)]
    port_range: RangeInclusive<u16>,

    /// Segredo compartilhado. Prefira a variável de ambiente: argumentos de
    /// linha de comando aparecem para outros usuários em `ps`.
    #[arg(long, env = "HBT_TOKEN", hide_env_values = true)]
    token: String,
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

/// Estado compartilhado entre todos os tasks.
///
/// `Arc<T>` = ponteiro com contagem de referências atômica: vários tasks
/// (possivelmente em threads diferentes) são "donos" do mesmo valor, que só
/// é liberado quando o último dono sai. É o jeito de compartilhar dados
/// entre `tokio::spawn`s, que exigem `'static` (não podem emprestar do
/// escopo que os criou).
struct State {
    token: String,
    tunnel_ip: IpAddr,
    port_range: RangeInclusive<u16>,
    /// Conexões públicas esperando o cliente "devolver" o lado de dados.
    ///
    /// `std::sync::Mutex` e não `tokio::sync::Mutex`: a seção crítica é só
    /// um insert/remove num HashMap (nanossegundos, sem `.await` dentro).
    /// O mutex do Tokio é mais caro e só compensa quando você precisa
    /// segurar o lock ATRAVÉS de um `.await`. O compilador te protege: um
    /// guard de std Mutex não é `Send`, então segurá-lo num await dentro de
    /// um `spawn` dá erro de compilação.
    pending: Mutex<HashMap<ConnId, oneshot::Sender<TcpStream>>>,
}

impl State {
    /// Cria um id novo + um "canal de uma entrega" (`oneshot`) por onde a
    /// conexão de dados será entregue a quem está esperando.
    fn expect_conn(&self) -> (ConnId, oneshot::Receiver<TcpStream>) {
        let id: ConnId = rand::random();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    fn forget(&self, id: ConnId) {
        self.pending.lock().unwrap().remove(&id);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let state = Arc::new(State {
        token: args.token,
        tunnel_ip: args.tunnel_ip,
        port_range: args.port_range,
        pending: Mutex::new(HashMap::new()),
    });

    let listener = TcpListener::bind(&args.control_bind)
        .await
        .with_context(|| format!("não consegui abrir {}", args.control_bind))?;
    info!(
        "controle em {} | portas públicas {:?}",
        args.control_bind, state.port_range
    );

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // Ex.: "too many open files". Não derrube o servidor.
                warn!("accept falhou: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let state = state.clone(); // clona o Arc (barato), não o State
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, state).await {
                debug!("{addr}: {e:#}");
            }
        });
    }
}

/// Decide, pela primeira mensagem, se é um cliente novo ou uma conexão de dados.
async fn handle_conn(mut stream: TcpStream, state: Arc<State>) -> Result<()> {
    let hello: Hello = timeout(Duration::from_secs(5), read_msg(&mut stream))
        .await
        .context("timeout esperando Hello")??;
    match hello {
        Hello::Accept(id) => {
            let waiter = state.pending.lock().unwrap().remove(&id);
            match waiter {
                // `send` devolve a stream se ninguém mais espera (timeout); ignoramos.
                Some(tx) => {
                    let _ = tx.send(stream);
                }
                None => bail!("Accept com id desconhecido"),
            }
            Ok(())
        }
        Hello::Control => {
            // Qualquer erro antes de "Registered" é reportado ao cliente.
            match timeout(Duration::from_secs(5), negotiate(&mut stream, &state)).await {
                Ok(Ok(bound)) => run_tunnel(stream, state, bound).await,
                Ok(Err(e)) => {
                    let _ = write_msg(&mut stream, &ServerMsg::Error(e.to_string())).await;
                    Err(e)
                }
                Err(_) => bail!("timeout no handshake"),
            }
        }
    }
}

/// O recurso público que o túnel expõe.
enum Bound {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

/// Autentica o cliente e abre a porta pública pedida.
async fn negotiate(stream: &mut TcpStream, state: &State) -> Result<Bound> {
    let nonce: [u8; 32] = rand::random();
    write_msg(stream, &ServerMsg::Challenge(nonce)).await?;

    let ClientMsg::Auth(mac) = read_msg(stream).await? else {
        bail!("esperava Auth");
    };
    if !verify_auth(&state.token, &nonce, &mac) {
        bail!("autenticação falhou");
    }
    write_msg(stream, &ServerMsg::AuthOk).await?;

    let ClientMsg::Register { proto, remote_port } = read_msg(stream).await? else {
        bail!("esperava Register");
    };
    if !state.port_range.contains(&remote_port) {
        bail!(
            "porta {remote_port} fora da faixa permitida {:?}",
            state.port_range
        );
    }

    let addr = (state.tunnel_ip, remote_port);
    let bound = match proto {
        Proto::Tcp => Bound::Tcp(TcpListener::bind(addr).await.context("porta TCP indisponível")?),
        Proto::Udp => Bound::Udp(UdpSocket::bind(addr).await.context("porta UDP indisponível")?),
    };
    write_msg(stream, &ServerMsg::Registered { remote_port }).await?;
    info!("túnel {proto:?} aberto na porta {remote_port}");
    Ok(bound)
}

/// Vive enquanto o cliente estiver conectado.
async fn run_tunnel(stream: TcpStream, state: Arc<State>, bound: Bound) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    // Vários produtores (listener TCP / pump UDP) -> um único consumidor
    // (este loop, dono da escrita no socket de controle). `mpsc` = multi
    // producer, single consumer. Evita dois tasks escrevendo ao mesmo tempo.
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(64);

    // Leitura do canal de controle vive em task próprio porque `read_msg`
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

    let mut tunnel = AbortOnDrop(match bound {
        Bound::Tcp(listener) => tokio::spawn(tcp_accept_loop(listener, state, tx.clone())),
        Bound::Udp(socket) => tokio::spawn(udp_tunnel(socket, state, tx.clone())),
    });

    let mut beat = interval(HEARTBEAT_EVERY);
    loop {
        tokio::select! {
            Some(msg) = rx.recv() => write_msg(&mut wr, &msg).await?,
            _ = beat.tick() => write_msg(&mut wr, &ServerMsg::Heartbeat).await?,
            _ = &mut reader.0 => break,
            _ = &mut tunnel.0 => break,
        }
    }
    info!("túnel encerrado");
    // Ao sair, `reader` e `tunnel` sofrem Drop => tasks abortados => o
    // listener é fechado e a porta pública liberada. Isso é RAII.
    Ok(())
}

async fn tcp_accept_loop(listener: TcpListener, state: Arc<State>, tx: mpsc::Sender<ServerMsg>) {
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
        let (id, rx) = state.expect_conn();
        if tx.send(ServerMsg::NewConn(id)).await.is_err() {
            return; // canal de controle morreu
        }
        let state = state.clone();
        tokio::spawn(async move {
            // Dá 10s para o cliente abrir a conexão de dados.
            match timeout(Duration::from_secs(10), rx).await {
                Ok(Ok(mut data)) => {
                    // Copia nos dois sentidos até um lado fechar.
                    match copy_bidirectional(&mut public, &mut data).await {
                        Ok((up, down)) => debug!("{peer}: fechou ({up}B ↑, {down}B ↓)"),
                        Err(e) => debug!("{peer}: {e}"),
                    }
                }
                _ => {
                    state.forget(id);
                    debug!("{peer}: cliente não devolveu a conexão a tempo");
                }
            }
        });
    }
}

/// UDP: um único stream TCP de dados carrega TODOS os datagramas do túnel.
async fn udp_tunnel(socket: UdpSocket, state: Arc<State>, tx: mpsc::Sender<ServerMsg>) {
    let (id, rx) = state.expect_conn();
    if tx.send(ServerMsg::NewUdpStream(id)).await.is_err() {
        return;
    }
    let data = match timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(s)) => s,
        _ => {
            state.forget(id);
            warn!("cliente não abriu o stream UDP");
            return;
        }
    };
    let _ = data.set_nodelay(true);
    let (rd, wr) = data.into_split();
    let socket = Arc::new(socket);

    // Dois sentidos concorrentes no mesmo task: o primeiro que terminar
    // (erro/fechamento) encerra o outro, porque o `select!` descarta o perdedor.
    tokio::select! {
        r = udp_to_stream(socket.clone(), wr) => debug!("udp->stream: {r:?}"),
        r = stream_to_udp(socket, BufReader::new(rd)) => debug!("stream->udp: {r:?}"),
    }
}

async fn udp_to_stream(socket: Arc<UdpSocket>, mut wr: tokio::net::tcp::OwnedWriteHalf) -> io::Result<()> {
    let mut buf = vec![0u8; 65535];
    loop {
        let (n, peer) = socket.recv_from(&mut buf).await?;
        write_msg(
            &mut wr,
            &UdpFrame {
                peer,
                data: buf[..n].to_vec(),
            },
        )
        .await?;
    }
}

async fn stream_to_udp(
    socket: Arc<UdpSocket>,
    mut rd: BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> io::Result<()> {
    loop {
        let f: UdpFrame = read_msg(&mut rd).await?;
        socket.send_to(&f.data, f.peer).await?;
    }
}
