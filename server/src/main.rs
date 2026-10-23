//! hbt-server: roda na nuvem (IP público). Aceita clientes na porta de
//! controle e abre portas públicas que redirecionam o tráfego pelo túnel.
//!
//! Três tipos de túnel: TCP e UDP (porta pública dedicada) e HTTP (uma porta
//! de entrada compartilhada; o `Host` da requisição escolhe o túnel).
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
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use hbt_proto::*;
use rand::Rng;
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt, BufReader};
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

    /// Domínio base dos túneis HTTP: `abc.<domain>` vai para o túnel `abc`.
    /// Sem isso, túneis HTTP ficam desabilitados.
    #[arg(long, env = "HBT_DOMAIN")]
    domain: Option<String>,

    /// Onde escutar requisições HTTP públicas (usado com --domain). A porta 80
    /// exige root ou `setcap cap_net_bind_service=+ep`. Para HTTPS, ponha um
    /// proxy com TLS (Caddy/nginx) na frente e use p.ex. 127.0.0.1:8080.
    #[arg(long, env = "HBT_HTTP_BIND", default_value = "0.0.0.0:80")]
    http_bind: String,

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
    /// Configuração do edge HTTP (None = desabilitado).
    http: Option<HttpCfg>,
    /// subdomínio -> canal de controle do cliente dono dele. O edge HTTP usa
    /// isto para pedir "abra uma conexão de dados" ao cliente certo.
    routes: Mutex<HashMap<String, mpsc::Sender<ServerMsg>>>,
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

struct HttpCfg {
    domain: String,
    /// Porta pública do edge, só para montar a URL mostrada ao usuário.
    port: u16,
}

/// Remove a rota HTTP quando o túnel acaba (RAII, como `AbortOnDrop`).
struct RouteGuard {
    state: Arc<State>,
    subdomain: String,
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.state.routes.lock().unwrap().remove(&self.subdomain);
    }
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

    let http_listener = match args.domain {
        Some(_) => Some(
            TcpListener::bind(&args.http_bind)
                .await
                .with_context(|| format!("não consegui abrir {}", args.http_bind))?,
        ),
        None => None,
    };
    let http = match (&args.domain, &http_listener) {
        (Some(domain), Some(l)) => Some(HttpCfg {
            domain: domain.to_ascii_lowercase(),
            port: l.local_addr()?.port(),
        }),
        _ => None,
    };

    let state = Arc::new(State {
        token: args.token,
        tunnel_ip: args.tunnel_ip,
        port_range: args.port_range,
        http,
        routes: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
    });

    let listener = TcpListener::bind(&args.control_bind)
        .await
        .with_context(|| format!("não consegui abrir {}", args.control_bind))?;
    info!(
        "controle em {} | portas públicas {:?}",
        args.control_bind, state.port_range
    );

    if let Some(l) = http_listener {
        info!(
            "HTTP em {} | *.{}",
            args.http_bind,
            state.http.as_ref().unwrap().domain
        );
        tokio::spawn(http_edge(l, state.clone()));
    }

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
            let (tx, rx) = mpsc::channel::<ServerMsg>(64);
            match timeout(Duration::from_secs(5), negotiate(&mut stream, &state, &tx)).await {
                Ok(Ok(bound)) => run_tunnel(stream, state, bound, tx, rx).await,
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
    /// Sem socket próprio: só mantém a rota registrada enquanto viver.
    Http(RouteGuard),
}

/// Autentica o cliente e abre a porta pública pedida.
async fn negotiate(
    stream: &mut TcpStream,
    state: &Arc<State>,
    tx: &mpsc::Sender<ServerMsg>,
) -> Result<Bound> {
    let nonce: [u8; 32] = rand::random();
    write_msg(stream, &ServerMsg::Challenge(nonce)).await?;

    let ClientMsg::Auth(mac) = read_msg(stream).await? else {
        bail!("esperava Auth");
    };
    if !verify_auth(&state.token, &nonce, &mac) {
        bail!("autenticação falhou");
    }
    write_msg(stream, &ServerMsg::AuthOk).await?;

    let ClientMsg::Register(spec) = read_msg(stream).await? else {
        bail!("esperava Register");
    };
    info!("registro: {spec:?}");
    let (bound, public) = match spec {
        TunnelSpec::Tcp { remote_port } => {
            let (l, port) = bind_port(state, remote_port, TcpListener::bind).await?;
            (Bound::Tcp(l), Public::Port(port))
        }
        TunnelSpec::Udp { remote_port } => {
            let (s, port) = bind_port(state, remote_port, UdpSocket::bind).await?;
            (Bound::Udp(s), Public::Port(port))
        }
        TunnelSpec::Http { subdomain } => register_http(state, tx, subdomain)?,
    };
    write_msg(stream, &ServerMsg::Registered { public }).await?;
    Ok(bound)
}

/// Abre a porta pedida (validando a faixa) ou, se o cliente não pediu
/// nenhuma, sorteia portas da faixa até achar uma livre.
///
/// Genérico sobre COMO abrir (`bind`): serve para TCP e UDP sem duplicar
/// código. `F` é qualquer função/closure `endereço -> Future<io::Result<T>>`;
/// `TcpListener::bind` e `UdpSocket::bind` se encaixam direto.
async fn bind_port<T, F, Fut>(state: &State, requested: Option<u16>, bind: F) -> Result<(T, u16)>
where
    F: Fn((IpAddr, u16)) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    if let Some(port) = requested {
        if !state.port_range.contains(&port) {
            bail!("porta {port} fora da faixa permitida {:?}", state.port_range);
        }
        let t = bind((state.tunnel_ip, port))
            .await
            .with_context(|| format!("porta {port} indisponível"))?;
        return Ok((t, port));
    }
    for _ in 0..100 {
        // `thread_rng()` não é Send: por isso o gerador vive só nesta linha,
        // nunca atravessa um `.await`.
        let port = rand::thread_rng().gen_range(state.port_range.clone());
        if let Ok(t) = bind((state.tunnel_ip, port)).await {
            return Ok((t, port));
        }
    }
    bail!("nenhuma porta livre na faixa {:?}", state.port_range)
}

/// Subdomínio vira parte de um nome DNS: só `a-z 0-9 -`, 1 a 63 caracteres,
/// sem hífen nas pontas (regra de "label" DNS).
fn validate_subdomain(s: &str) -> Result<()> {
    let ok = (1..=63).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-');
    if !ok {
        bail!("subdomínio inválido (use a-z, 0-9 e '-', até 63 caracteres)");
    }
    Ok(())
}

fn register_http(
    state: &Arc<State>,
    tx: &mpsc::Sender<ServerMsg>,
    requested: Option<String>,
) -> Result<(Bound, Public)> {
    let Some(cfg) = &state.http else {
        bail!("túneis HTTP desabilitados neste servidor (falta --domain)");
    };
    if let Some(s) = &requested {
        validate_subdomain(s)?;
    }
    // Checar e inserir sob o MESMO lock: senão dois clientes poderiam
    // checar "livre" ao mesmo tempo e ambos registrar (race condition).
    let mut routes = state.routes.lock().unwrap();
    let sub = match requested {
        Some(s) => s,
        None => loop {
            let candidate = format!("{:08x}", rand::random::<u32>());
            if !routes.contains_key(&candidate) {
                break candidate;
            }
        },
    };
    if routes.contains_key(&sub) {
        bail!("subdomínio '{sub}' já está em uso");
    }
    routes.insert(sub.clone(), tx.clone());
    drop(routes);

    let port = if cfg.port == 80 {
        String::new()
    } else {
        format!(":{}", cfg.port)
    };
    let url = format!("http://{sub}.{}{port}", cfg.domain);
    let guard = RouteGuard {
        state: state.clone(),
        subdomain: sub,
    };
    Ok((Bound::Http(guard), Public::Url(url)))
}

/// Vive enquanto o cliente estiver conectado.
async fn run_tunnel(
    stream: TcpStream,
    state: Arc<State>,
    bound: Bound,
    tx: mpsc::Sender<ServerMsg>,
    mut rx: mpsc::Receiver<ServerMsg>,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    // `rx` é o único consumidor (este loop, dono da escrita no socket de
    // controle); produtores: listener TCP, pump UDP ou o edge HTTP (via
    // `routes`). `mpsc` = multi producer, single consumer: evita dois tasks
    // escrevendo ao mesmo tempo no mesmo socket.

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

    // Túneis TCP/UDP têm um task; HTTP só uma rota (guard) que vive até o fim.
    let mut tunnel = None;
    let mut _route = None; // com nome: o guard só cai quando a função termina
    match bound {
        Bound::Tcp(l) => tunnel = Some(AbortOnDrop(tokio::spawn(tcp_accept_loop(l, state, tx)))),
        Bound::Udp(s) => tunnel = Some(AbortOnDrop(tokio::spawn(udp_tunnel(s, state, tx)))),
        Bound::Http(guard) => _route = Some(guard),
    }

    let mut beat = interval(HEARTBEAT_EVERY);
    loop {
        tokio::select! {
            Some(msg) = rx.recv() => write_msg(&mut wr, &msg).await?,
            _ = beat.tick() => write_msg(&mut wr, &ServerMsg::Heartbeat).await?,
            _ = &mut reader.0 => break,
            _ = tunnel_done(&mut tunnel) => break,
        }
    }
    info!("túnel encerrado");
    // Ao sair, `reader` e `tunnel` sofrem Drop => tasks abortados => o
    // listener é fechado e a porta pública liberada. Isso é RAII.
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

// ------------------------------------------------------------ edge HTTP

/// Tamanho máximo da linha de requisição + cabeçalhos que aceitamos ler.
const MAX_HEAD: usize = 16 * 1024;

async fn http_edge(listener: TcpListener, state: Arc<State>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("accept http falhou: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_http(stream, state).await {
                debug!("http {peer}: {e:#}");
            }
        });
    }
}

/// Lê os cabeçalhos para descobrir o `Host`, escolhe o túnel e depois vira
/// um simples "cano" TCP: reenviamos os bytes já lidos e copiamos o resto.
///
/// Trade-off: roteamos por CONEXÃO, olhando só a primeira requisição. Não
/// reescrevemos nem inspecionamos requisições seguintes (keep-alive). Navegadores
/// usam uma conexão por origem, então funciona; um proxy que misture vários
/// hosts numa conexão só seria roteado pelo primeiro. Em troca, WebSocket,
/// streaming e uploads grandes funcionam sem esforço (não parseamos corpos).
async fn handle_http(mut public: TcpStream, state: Arc<State>) -> Result<()> {
    let (head, host) = match timeout(Duration::from_secs(10), read_http_head(&mut public)).await {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => {
            respond(&mut public, "400 Bad Request", "requisição HTTP inválida").await;
            return Err(e);
        }
        Err(_) => bail!("timeout lendo cabeçalhos"),
    };

    let route = subdomain_of(&state, &host).and_then(|sub| state.routes.lock().unwrap().get(&sub).cloned());
    let Some(route) = route else {
        respond(
            &mut public,
            "404 Not Found",
            &format!("nenhum túnel para '{host}'"),
        )
        .await;
        return Ok(());
    };

    let (id, rx) = state.expect_conn();
    if route.send(ServerMsg::NewConn(id)).await.is_err() {
        state.forget(id);
        respond(&mut public, "502 Bad Gateway", "túnel offline").await;
        return Ok(());
    }
    match timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(mut data)) => {
            data.write_all(&head).await?; // replay do que já consumimos
            copy_bidirectional(&mut public, &mut data).await?;
        }
        _ => {
            state.forget(id);
            respond(
                &mut public,
                "504 Gateway Timeout",
                "o cliente do túnel não respondeu",
            )
            .await;
        }
    }
    Ok(())
}

/// `Abc.exemplo.com:8080` -> `abc` (se o domínio base bater e for 1 nível só).
fn subdomain_of(state: &State, host: &str) -> Option<String> {
    let cfg = state.http.as_ref()?;
    let host = host.to_ascii_lowercase();
    // Tira ":porta" (só se o que vem depois dos ':' for número).
    let name = match host.rsplit_once(':') {
        Some((n, port)) if port.bytes().all(|b| b.is_ascii_digit()) => n,
        _ => host.as_str(),
    };
    let sub = name.strip_suffix(&format!(".{}", cfg.domain))?;
    (!sub.contains('.')).then(|| sub.to_owned())
}

/// Acumula bytes até os cabeçalhos HTTP/1.x estarem completos e devolve
/// (bytes lidos, valor do Host). `httparse` é o mesmo parser usado pelo hyper:
/// parsear HTTP à mão é fonte clássica de bugs de segurança (request smuggling).
async fn read_http_head(s: &mut TcpStream) -> Result<(Vec<u8>, String)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            bail!("conexão fechada antes dos cabeçalhos");
        }
        buf.extend_from_slice(&chunk[..n]);

        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf)? {
            httparse::Status::Complete(_) => {
                let host = req
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("host"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .map(str::to_owned)
                    .context("sem cabeçalho Host")?;
                return Ok((buf, host));
            }
            httparse::Status::Partial if buf.len() >= MAX_HEAD => bail!("cabeçalhos grandes demais"),
            httparse::Status::Partial => {}
        }
    }
}

async fn respond(s: &mut TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}\n",
        body.len() + 1
    );
    let _ = s.write_all(msg.as_bytes()).await;
}
