//! hbt-server: roda na nuvem (IP público). Aceita clientes na porta de
//! controle e abre portas públicas que redirecionam o tráfego pelo túnel.
//!
//! Três tipos de túnel: TCP e UDP (porta pública dedicada) e HTTP (uma porta
//! de entrada compartilhada; o `Host` da requisição escolhe o túnel).
//!
//! Fluxo (resumo; detalhes em docs/GUIDE.md):
//!
//!   cliente conecta na porta de controle, autentica e registra o túnel;
//!   a partir daí essa MESMA conexão vira uma sessão multiplexada (yamux).
//!
//!   usuário --> [porta pública no servidor]
//!                   |  servidor abre um stream novo na sessão (sem handshake)
//!                   v
//!               cliente recebe o stream, conecta no serviço local e cola as pontas

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod metrics;
mod tokens;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use hbt_proto::mux::{self, Mode, MuxStream, Opener, StreamKind};
use hbt_proto::*;
use metrics::{ConnError, Kind, Metrics};
use rand::Rng;
use tokens::{TokenRecord, TokenStore};
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::{interval, timeout};
use tracing::{debug, info, warn};

#[derive(Parser)]
#[command(version, about = "hbt reverse tunnel server")]
struct Cli {
    /// Diretório de dados (guarda tokens.json). Precisa ser gravável.
    #[arg(long, env = "HBT_DATA_DIR", default_value = "/var/lib/hbt", global = true)]
    data_dir: PathBuf,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Roda o servidor.
    Serve(ServeArgs),
    /// Gerencia os tokens dos clientes (valem imediatamente, sem reiniciar).
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Cria um token. Ele é mostrado UMA vez; o servidor não guarda o segredo.
    Create {
        /// Nome do cliente/dispositivo, ex: notebook-ana
        name: String,
        /// Limita as portas TCP/UDP que este token pode abrir, ex: 20000-20100
        #[arg(long, value_parser = parse_range)]
        ports: Option<RangeInclusive<u16>>,
        /// Limita os subdomínios HTTP (separados por vírgula), ex: ana,ana-dev
        #[arg(long, value_delimiter = ',')]
        subdomains: Option<Vec<String>>,
        /// Imprime só o token (para scripts).
        #[arg(short, long)]
        quiet: bool,
    },
    /// Lista os tokens (sem segredos).
    List,
    /// Revoga um token por nome ou id; sessões ativas caem em até ~10s.
    Revoke { name_or_id: String },
}

#[derive(Parser)]
struct ServeArgs {
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

    /// Onde servir métricas Prometheus (`GET /metrics`), ex: 127.0.0.1:9477.
    /// Sem isso, desligado. Não há autenticação: não exponha à internet.
    #[arg(long, env = "HBT_METRICS_BIND")]
    metrics_bind: Option<String>,
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

fn token_admin(store: &TokenStore, cmd: TokenCmd) -> Result<()> {
    match cmd {
        TokenCmd::Create {
            name,
            ports,
            subdomains,
            quiet,
        } => {
            let token = store.create(&name, ports, subdomains)?;
            if quiet {
                println!("{token}");
            } else {
                println!("Token '{name}' criado. Guarde-o agora: ele não será mostrado de novo.\n");
                println!("  {token}\n");
                println!("No cliente:  hbt configure --server SEU_SERVIDOR --token {token}");
            }
        }
        TokenCmd::List => tokens::print_table(&store.load()?),
        TokenCmd::Revoke { name_or_id } => {
            let r = store.revoke(&name_or_id)?;
            println!("Token '{}' ({}) revogado.", r.name, r.id);
        }
    }
    Ok(())
}

/// Estado compartilhado entre todos os tasks.
///
/// `Arc<T>` = ponteiro com contagem de referências atômica: vários tasks
/// (possivelmente em threads diferentes) são "donos" do mesmo valor, que só
/// é liberado quando o último dono sai. É o jeito de compartilhar dados
/// entre `tokio::spawn`s, que exigem `'static` (não podem emprestar do
/// escopo que os criou).
struct State {
    tokens: TokenStore,
    tunnel_ip: IpAddr,
    port_range: RangeInclusive<u16>,
    /// Configuração do edge HTTP (None = desabilitado).
    http: Option<HttpCfg>,
    /// subdomínio -> sessão do cliente dono dele. O edge HTTP usa isto para
    /// abrir um stream até o cliente certo.
    ///
    /// `std::sync::Mutex` e não `tokio::sync::Mutex`: a seção crítica é só
    /// um get/insert num HashMap (nanossegundos, sem `.await` dentro).
    /// O mutex do Tokio é mais caro e só compensa quando você precisa
    /// segurar o lock ATRAVÉS de um `.await`. O compilador te protege: um
    /// guard de std Mutex não é `Send`, então segurá-lo num await dentro de
    /// um `spawn` dá erro de compilação.
    routes: Mutex<HashMap<String, Opener>>,
    metrics: Arc<Metrics>,
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let store = TokenStore::new(&cli.data_dir);
    match cli.cmd {
        Command::Token { cmd } => token_admin(&store, cmd),
        Command::Serve(args) => serve(args, store).await,
    }
}

async fn serve(args: ServeArgs, tokens: TokenStore) -> Result<()> {
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
        tokens,
        tunnel_ip: args.tunnel_ip,
        port_range: args.port_range,
        http,
        routes: Mutex::new(HashMap::new()),
        metrics: Arc::default(),
    });

    let listener = TcpListener::bind(&args.control_bind)
        .await
        .with_context(|| format!("não consegui abrir {}", args.control_bind))?;
    info!(
        "controle em {} | portas públicas {:?}",
        args.control_bind, state.port_range
    );
    match state.tokens.find("").await {
        Err(e) => bail!("não consigo ler os tokens: {e:#}"),
        Ok(_) if state.tokens.load()?.is_empty() => warn!(
            "nenhum token cadastrado: ninguém consegue conectar. Crie com: hbt-server token create <nome>"
        ),
        Ok(_) => {}
    }

    if let Some(bind) = &args.metrics_bind {
        let l = TcpListener::bind(bind)
            .await
            .with_context(|| format!("não consegui abrir {bind}"))?;
        info!("métricas em http://{bind}/metrics");
        tokio::spawn(metrics::serve(l, state.metrics.clone()));
    }

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

const OLD_CLIENT: &str = "este cliente hbt é antigo (uma conexão por usuário); \
     atualize o hbt para a versão com multiplexação";

/// Uma conexão na porta de controle: handshake e, se der certo, o túnel.
async fn handle_conn(mut stream: TcpStream, state: Arc<State>) -> Result<()> {
    let hello: Hello = timeout(Duration::from_secs(5), read_msg(&mut stream))
        .await
        .context("timeout esperando Hello")??;
    match hello {
        // O cliente antigo só mostra (e desiste com) um `Error` recebido
        // DEPOIS do desafio; antes disso ele só tentaria de novo para sempre.
        Hello::V1Control { .. } => {
            state.metrics.handshake_failed();
            write_msg(&mut stream, &ServerMsg::Challenge(rand::random())).await?;
            let _ = timeout(Duration::from_secs(5), read_msg::<_, ClientMsg>(&mut stream)).await;
            let _ = write_msg(&mut stream, &ServerMsg::Error(OLD_CLIENT.into())).await;
            bail!("cliente com protocolo antigo recusado")
        }
        Hello::V1Accept(_) => bail!("conexão de dados do protocolo antigo"),
        Hello::Control { token_id } => {
            // Qualquer erro antes de "Registered" é reportado ao cliente.
            let (opener, requests) = mux::opener();
            match timeout(
                Duration::from_secs(5),
                negotiate(&mut stream, &state, &opener, &token_id),
            )
            .await
            {
                Ok(Ok((bound, rec))) => run_tunnel(stream, state, bound, rec, opener, requests).await,
                Ok(Err(e)) => {
                    state.metrics.handshake_failed();
                    let _ = write_msg(&mut stream, &ServerMsg::Error(e.to_string())).await;
                    Err(e)
                }
                Err(_) => {
                    state.metrics.handshake_failed();
                    bail!("timeout no handshake")
                }
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
    opener: &Opener,
    token_id: &str,
) -> Result<(Bound, TokenRecord)> {
    // Id desconhecido recebe o desafio igual a um id válido e falha só depois
    // da verificação, com a MESMA mensagem: assim não dá para descobrir quais
    // ids existem (user enumeration) olhando respostas ou tempo.
    let record = state.tokens.find(token_id).await?;
    let key = record
        .as_ref()
        .and_then(|r| hex_decode(&r.key))
        .unwrap_or_else(|| vec![0u8; 32]);

    let nonce: [u8; 32] = rand::random();
    write_msg(stream, &ServerMsg::Challenge(nonce)).await?;

    let ClientMsg::Auth(mac) = read_msg(stream).await? else {
        bail!("esperava Auth");
    };
    let valid = verify_auth(&key, &nonce, &mac);
    let Some(record) = record.filter(|_| valid) else {
        state.metrics.auth_failed();
        bail!("autenticação falhou");
    };
    write_msg(stream, &ServerMsg::AuthOk).await?;
    info!("cliente '{}' autenticado", record.name);

    let ClientMsg::Register(spec) = read_msg(stream).await? else {
        bail!("esperava Register");
    };
    info!("registro de '{}': {spec:?}", record.name);
    let (bound, public) = match spec {
        TunnelSpec::Tcp { remote_port } => {
            let range = port_range_for(state, &record)?;
            let (l, port) = bind_port(state.tunnel_ip, &range, remote_port, TcpListener::bind).await?;
            (Bound::Tcp(l), Public::Port(port))
        }
        TunnelSpec::Udp { remote_port } => {
            let range = port_range_for(state, &record)?;
            let (s, port) = bind_port(state.tunnel_ip, &range, remote_port, UdpSocket::bind).await?;
            (Bound::Udp(s), Public::Port(port))
        }
        TunnelSpec::Http { subdomain } => register_http(state, opener, subdomain, &record)?,
    };
    write_msg(stream, &ServerMsg::Registered { public }).await?;
    Ok((bound, record))
}

/// Faixa que ESTE cliente pode usar: faixa global do servidor ∩ faixa do token.
fn port_range_for(state: &State, record: &TokenRecord) -> Result<RangeInclusive<u16>> {
    record.effective_ports(&state.port_range).with_context(|| {
        format!(
            "o token '{}' não permite nenhuma porta da faixa do servidor",
            record.name
        )
    })
}

/// Abre a porta pedida (validando a faixa) ou, se o cliente não pediu
/// nenhuma, sorteia portas da faixa até achar uma livre.
///
/// Genérico sobre COMO abrir (`bind`): serve para TCP e UDP sem duplicar
/// código. `F` é qualquer função/closure `endereço -> Future<io::Result<T>>`;
/// `TcpListener::bind` e `UdpSocket::bind` se encaixam direto.
async fn bind_port<T, F, Fut>(
    ip: IpAddr,
    range: &RangeInclusive<u16>,
    requested: Option<u16>,
    bind: F,
) -> Result<(T, u16)>
where
    F: Fn((IpAddr, u16)) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    if let Some(port) = requested {
        if !range.contains(&port) {
            bail!("porta {port} fora da faixa permitida para este token ({range:?})");
        }
        let t = bind((ip, port))
            .await
            .with_context(|| format!("porta {port} indisponível"))?;
        return Ok((t, port));
    }
    for _ in 0..100 {
        // `thread_rng()` não é Send: por isso o gerador vive só nesta linha,
        // nunca atravessa um `.await`.
        let port = rand::thread_rng().gen_range(range.clone());
        if let Ok(t) = bind((ip, port)).await {
            return Ok((t, port));
        }
    }
    bail!("nenhuma porta livre na faixa {range:?}")
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
    opener: &Opener,
    requested: Option<String>,
    record: &TokenRecord,
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
    // Token com lista de subdomínios: só estes. Sem pedir nenhum, usa o
    // primeiro livre da lista (em vez de sortear um que não seria dele).
    let sub = match (requested, &record.subdomains) {
        (Some(s), Some(list)) if !list.contains(&s) => {
            bail!("subdomínio '{s}' não é permitido para o token '{}'", record.name)
        }
        (Some(s), _) => s,
        (None, Some(list)) => list
            .iter()
            .find(|c| !routes.contains_key(*c))
            .cloned()
            .context("todos os subdomínios deste token estão em uso")?,
        (None, None) => loop {
            let candidate = format!("{:08x}", rand::random::<u32>());
            if !routes.contains_key(&candidate) {
                break candidate;
            }
        },
    };
    if routes.contains_key(&sub) {
        bail!("subdomínio '{sub}' já está em uso");
    }
    routes.insert(sub.clone(), opener.clone());
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
    record: TokenRecord,
    opener: Opener,
    requests: mux::Requests,
) -> Result<()> {
    // Daqui em diante a conexão é uma sessão multiplexada: um task a "gira"
    // (proto/src/mux.rs) e todos os outros só abrem streams nela.
    let mut session = AbortOnDrop(tokio::spawn(async move {
        if let Err(e) = mux::run(stream, Mode::Server, requests, None).await {
            debug!("sessão: {e}");
        }
    }));
    // O primeiro stream é o de controle: heartbeats nos dois sentidos.
    let ctl = opener.open(StreamKind::Control).await?;
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
        Bound::Udp(s) => tunnel = Some(AbortOnDrop(tokio::spawn(udp_tunnel(s, state.clone(), opener)))),
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
            _ = &mut session.0 => break,
        }
    }
    info!("túnel de '{}' encerrado", record.name);
    // Ao sair, `reader`, `tunnel` e `session` sofrem Drop => tasks abortados
    // => o listener é fechado, a porta pública liberada e a conexão com o
    // cliente cai (derrubando todos os streams). Isso é RAII.
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
async fn open_conn(opener: &Opener, metrics: &Metrics) -> Result<MuxStream, ConnError> {
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

/// UDP: um único stream da sessão carrega TODOS os datagramas do túnel.
async fn udp_tunnel(socket: UdpSocket, state: Arc<State>, opener: Opener) {
    let data = match timeout(Duration::from_secs(10), opener.open(StreamKind::Udp)).await {
        Ok(Ok(s)) => s,
        _ => {
            warn!("não consegui abrir o stream UDP até o cliente");
            return;
        }
    };
    let (rd, wr) = tokio::io::split(data);
    let socket = Arc::new(socket);

    // Dois sentidos concorrentes no mesmo task: o primeiro que terminar
    // (erro/fechamento) encerra o outro, porque o `select!` descarta o perdedor.
    tokio::select! {
        r = udp_to_stream(socket.clone(), wr, &state.metrics) => debug!("udp->stream: {r:?}"),
        r = stream_to_udp(socket, BufReader::new(rd), &state.metrics) => debug!("stream->udp: {r:?}"),
    }
}

async fn udp_to_stream(
    socket: Arc<UdpSocket>,
    mut wr: tokio::io::WriteHalf<MuxStream>,
    metrics: &Metrics,
) -> io::Result<()> {
    let mut buf = vec![0u8; 65535];
    loop {
        let (n, peer) = socket.recv_from(&mut buf).await?;
        metrics.udp_in(n);
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
    mut rd: BufReader<tokio::io::ReadHalf<MuxStream>>,
    metrics: &Metrics,
) -> io::Result<()> {
    loop {
        let f: UdpFrame = read_msg(&mut rd).await?;
        socket.send_to(&f.data, f.peer).await?;
        metrics.udp_out(f.data.len());
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
    // Sem isso, Nagle + delayed ACK seguram respostas pequenas em keep-alive (~40ms).
    let _ = public.set_nodelay(true);
    let _conn = state.metrics.conn(Kind::Http);
    let (head, host) = match timeout(Duration::from_secs(10), read_http_head(&mut public)).await {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => {
            state.metrics.conn_error(ConnError::BadRequest);
            respond(&mut public, "400 Bad Request", "requisição HTTP inválida").await;
            return Err(e);
        }
        Err(_) => bail!("timeout lendo cabeçalhos"),
    };

    let route = subdomain_of(&state, &host).and_then(|sub| state.routes.lock().unwrap().get(&sub).cloned());
    let Some(route) = route else {
        state.metrics.conn_error(ConnError::NoRoute);
        respond(
            &mut public,
            "404 Not Found",
            &format!("nenhum túnel para '{host}'"),
        )
        .await;
        return Ok(());
    };

    match open_conn(&route, &state.metrics).await {
        Ok(mut data) => {
            data.write_all(&head).await?; // replay do que já consumimos
            let (up, down) = copy_bidirectional(&mut public, &mut data).await?;
            state.metrics.bytes(Kind::Http, head.len() as u64 + up, down);
        }
        Err(ConnError::TunnelOffline) => respond(&mut public, "502 Bad Gateway", "túnel offline").await,
        Err(_) => {
            respond(
                &mut public,
                "504 Gateway Timeout",
                "o túnel está no limite de conexões simultâneas",
            )
            .await
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
