//! Testes de ponta a ponta: sobem os binários reais e passam tráfego de verdade.
//! Rode com: cargo test -p hbt-e2e

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

/// Compila os binários uma vez e devolve o diretório onde ficam.
fn bin_dir() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = std::process::Command::new(cargo)
            .args(["build", "-q", "-p", "hbt-server", "-p", "hbt"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "falha ao compilar os binários");
    });
    // .../target/debug/deps/tunnel-xxxx  =>  .../target/debug
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p
}

/// Pede uma porta livre ao SO (bind na porta 0) e a devolve.
/// Há uma pequena janela de corrida até o processo usá-la; aceitável em testes.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Comando de um binário do projeto, com ambiente limpo de HBT_* herdados.
fn cmd(bin: &str) -> Command {
    let mut c = Command::new(bin_dir().join(bin));
    c.env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER")
        .env_remove("HBT_DOMAIN")
        .env_remove("HBT_DATA_DIR")
        .kill_on_drop(true); // sem isso, falha de teste deixaria processos órfãos
    c
}

fn spawn_quiet(mut c: Command) -> Child {
    c.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
}

/// Servidor de teste: diretório de dados próprio + processo `serve`.
struct Fixture {
    dir: PathBuf,
    control: u16,
    _server: Child,
}

impl Fixture {
    /// `range` = faixa pública de portas; `extra` = flags extras do `serve`.
    fn start(range: &str, extra: &[&str]) -> Fixture {
        let control = free_port();
        let dir = std::env::temp_dir().join(format!("hbt-e2e-{}-{control}", std::process::id()));
        let mut c = cmd("hbt-server");
        c.args(["--data-dir"]).arg(&dir);
        c.args(["serve", "--control-bind", &format!("127.0.0.1:{control}")]);
        c.args(["--tunnel-ip", "127.0.0.1", "--port-range", range]);
        c.args(extra);
        Fixture {
            dir,
            control,
            _server: spawn_quiet(c),
        }
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.control)
    }

    /// `hbt-server token <args...>` e devolve o stdout.
    fn token_cmd(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(bin_dir().join("hbt-server"))
            .arg("--data-dir")
            .arg(&self.dir)
            .arg("token")
            .args(args)
            .output()
            .unwrap()
    }

    /// Cria um token e devolve a string `<id>.<segredo>`.
    fn token(&self, name: &str, extra: &[&str]) -> String {
        let mut args = vec!["create", name, "--quiet"];
        args.extend_from_slice(extra);
        let out = self.token_cmd(&args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn client(&self, token: &str, args: &[&str]) -> Command {
        let mut c = cmd("hbt");
        c.args(["--server", &self.addr()])
            .args(args)
            .env("HBT_TOKEN", token);
        c
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn tcp_echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

async fn udp_echo_server() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = s.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = s.recv_from(&mut buf).await {
            let _ = s.send_to(&buf[..n], from).await;
        }
    });
    port
}

/// Tenta por até ~10s fazer um eco "ping" pelo túnel TCP.
async fn tcp_echo_works(port: u16) -> bool {
    for _ in 0..50 {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await {
            if s.write_all(b"ping").await.is_ok() {
                let mut buf = [0u8; 4];
                if let Ok(Ok(_)) = timeout(Duration::from_millis(500), s.read_exact(&mut buf)).await {
                    return &buf == b"ping";
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Sobe o cliente com stdout capturado e devolve a URL do banner "Forwarding".
async fn banner_url(mut c: Command) -> (Child, String) {
    let mut child = c.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let url = timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Some(rest) = line.strip_prefix("Forwarding") {
                return rest.split_whitespace().next().unwrap().to_owned();
            }
        }
        panic!("cliente terminou sem imprimir o banner");
    })
    .await
    .expect("sem banner em 10s");
    (child, url)
}

// ------------------------------------------------------------------ túneis

#[tokio::test]
async fn tcp_tunnel_echoes_multiple_connections() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let echo = tcp_echo_server().await;
    let _client = spawn_quiet(fx.client(
        &token,
        &["tcp", &echo.to_string(), "--remote-port", &public.to_string()],
    ));
    assert!(tcp_echo_works(public).await, "túnel TCP nunca respondeu");

    // Várias conexões simultâneas, cada uma com seu payload.
    let addr: SocketAddr = format!("127.0.0.1:{public}").parse().unwrap();
    let mut tasks = vec![];
    for i in 0..10u8 {
        tasks.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            let msg = vec![i; 100_000]; // grande: força vários reads/writes
            let (mut r, mut w) = s.split();
            let write = async { w.write_all(&msg).await.unwrap() };
            let mut got = vec![0u8; msg.len()];
            let read = async { r.read_exact(&mut got).await.unwrap() };
            tokio::join!(write, read);
            assert_eq!(got, msg);
        }));
    }
    for t in tasks {
        timeout(Duration::from_secs(10), t).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn udp_tunnel_echoes_and_keeps_peers_separate() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let echo = udp_echo_server().await;
    let _client = spawn_quiet(fx.client(
        &token,
        &["udp", &echo.to_string(), "--remote-port", &public.to_string()],
    ));

    let dest = format!("127.0.0.1:{public}");
    // Dois peers distintos: cada um deve receber SÓ a própria resposta.
    for tag in [b"alpha".as_slice(), b"bravo".as_slice()] {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut buf = [0u8; 64];
        let mut got = None;
        for _ in 0..50 {
            sock.send_to(tag, &dest).await.unwrap();
            if let Ok(Ok((n, _))) = timeout(Duration::from_millis(200), sock.recv_from(&mut buf)).await {
                got = Some(buf[..n].to_vec());
                break;
            }
        }
        assert_eq!(got.as_deref(), Some(tag), "túnel UDP não devolveu o eco");
    }
}

#[tokio::test]
async fn server_picks_a_random_port_when_none_requested() {
    let lo = free_port();
    let hi = lo.saturating_add(20);
    let fx = Fixture::start(&format!("{lo}-{hi}"), &[]);
    let token = fx.token("t", &[]);
    let echo = tcp_echo_server().await;
    let (_client, url) = banner_url(fx.client(&token, &["tcp", &echo.to_string()])).await;

    let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
    assert!((lo..=hi).contains(&port), "porta {port} fora de {lo}-{hi}");
    assert!(tcp_echo_works(port).await);
}

#[tokio::test]
async fn port_outside_server_range_is_refused() {
    let (allowed, denied) = (free_port(), free_port());
    let fx = Fixture::start(&format!("{allowed}-{allowed}"), &[]);
    let token = fx.token("t", &[]);
    let _client = spawn_quiet(fx.client(&token, &["tcp", "1", "--remote-port", &denied.to_string()]));
    sleep(Duration::from_millis(1500)).await;
    assert!(TcpStream::connect(("127.0.0.1", denied)).await.is_err());
}

// -------------------------------------------------------------------- http

/// Servidor HTTP mínimo: responde "ok" a qualquer requisição.
async fn http_ok_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .await;
            });
        }
    });
    port
}

/// Faz um GET cru para o edge HTTP com o `Host` dado e devolve a resposta inteira
/// (vazia se o servidor ainda está subindo).
async fn http_get(edge: u16, host: &str) -> String {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", edge)).await else {
        return String::new();
    };
    let req = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    let _ = timeout(Duration::from_secs(5), s.read_to_string(&mut out)).await;
    out
}

/// Repete o GET até a resposta conter `needle` (ou ~10s).
async fn http_wait(edge: u16, host: &str, needle: &str) -> String {
    let mut resp = String::new();
    for _ in 0..50 {
        resp = http_get(edge, host).await;
        if resp.contains(needle) {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    resp
}

/// Servidor com HTTP em `localhost`; devolve (fixture, porta do edge).
fn http_fixture() -> (Fixture, u16) {
    let edge = free_port();
    let public = free_port();
    let fx = Fixture::start(
        &format!("{public}-{public}"),
        &[
            "--domain",
            "localhost",
            "--http-bind",
            &format!("127.0.0.1:{edge}"),
        ],
    );
    (fx, edge)
}

#[tokio::test]
async fn http_tunnel_routes_by_host_header() {
    let (fx, edge) = http_fixture();
    let token = fx.token("t", &[]);
    let web = http_ok_server().await;
    let _client = spawn_quiet(fx.client(&token, &["http", &web.to_string(), "--subdomain", "demo"]));

    // Com :porta no Host também deve rotear.
    let resp = http_wait(edge, &format!("demo.localhost:{edge}"), "200 OK").await;
    assert!(
        resp.starts_with("HTTP/1.1 200 OK") && resp.ends_with("ok"),
        "resposta: {resp:?}"
    );
    assert!(
        http_get(edge, "DEMO.localhost").await.contains("200 OK"),
        "Host é case-insensitive"
    );

    // Subdomínio sem túnel e domínio errado => 404.
    assert!(http_get(edge, "outro.localhost")
        .await
        .starts_with("HTTP/1.1 404"));
    assert!(http_get(edge, "demo.exemplo.com")
        .await
        .starts_with("HTTP/1.1 404"));
}

#[tokio::test]
async fn http_is_refused_when_server_has_no_domain() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let mut client = spawn_quiet(fx.client(&token, &["http", "1"]));
    // Recusa é não-fatal (o cliente tenta de novo), então ele NÃO deve sair sozinho.
    assert!(timeout(Duration::from_secs(2), client.wait()).await.is_err());
}

// ------------------------------------------------------- autenticação / ACL

#[tokio::test]
async fn wrong_secret_makes_client_exit_with_error() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let (id, _) = token.split_once('.').unwrap();
    let forged = format!("{id}.{}", "0".repeat(64)); // id certo, segredo errado
    let mut client = spawn_quiet(fx.client(&forged, &["tcp", "1"]));
    let st = timeout(Duration::from_secs(10), client.wait())
        .await
        .expect("cliente deveria sair")
        .unwrap();
    assert!(!st.success());
}

#[tokio::test]
async fn unknown_token_id_is_rejected_like_a_wrong_secret() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    fx.token("t", &[]);
    let forged = format!("deadbeef.{}", "1".repeat(64));
    let mut client = spawn_quiet(fx.client(&forged, &["tcp", "1"]));
    let st = timeout(Duration::from_secs(10), client.wait())
        .await
        .expect("cliente deveria sair")
        .unwrap();
    assert!(!st.success());
}

#[tokio::test]
async fn token_port_acl_is_enforced() {
    let base = free_port();
    let (ok_port, bad_port) = (base, base.saturating_add(1));
    // Servidor permite as duas; o TOKEN só permite a primeira.
    let fx = Fixture::start(&format!("{ok_port}-{bad_port}"), &[]);
    let token = fx.token("restrito", &["--ports", &format!("{ok_port}-{ok_port}")]);
    let echo = tcp_echo_server().await;
    let e = echo.to_string();

    let _denied = spawn_quiet(fx.client(&token, &["tcp", &e, "--remote-port", &bad_port.to_string()]));
    sleep(Duration::from_millis(1500)).await;
    assert!(
        TcpStream::connect(("127.0.0.1", bad_port)).await.is_err(),
        "porta fora do token abriu"
    );

    let _allowed = spawn_quiet(fx.client(&token, &["tcp", &e, "--remote-port", &ok_port.to_string()]));
    assert!(
        tcp_echo_works(ok_port).await,
        "porta permitida pelo token não funcionou"
    );
}

#[tokio::test]
async fn token_subdomain_acl_is_enforced() {
    let (fx, edge) = http_fixture();
    let token = fx.token("ana", &["--subdomains", "ana"]);
    let web = http_ok_server().await;
    let w = web.to_string();

    // Pedir subdomínio fora da lista: nunca roteia.
    let mut denied = spawn_quiet(fx.client(&token, &["http", &w, "--subdomain", "outro"]));
    sleep(Duration::from_millis(1500)).await;
    assert!(http_get(edge, "outro.localhost")
        .await
        .starts_with("HTTP/1.1 404"));
    let _ = denied.kill().await;

    // Sem pedir nenhum: o servidor usa o da lista ("ana").
    let _ok = spawn_quiet(fx.client(&token, &["http", &w]));
    let resp = http_wait(edge, "ana.localhost", "200 OK").await;
    assert!(resp.contains("200 OK"), "resposta: {resp:?}");
}

#[tokio::test]
async fn revoking_a_token_drops_the_live_session_and_blocks_reconnect() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("vai-ser-revogado", &[]);
    let echo = tcp_echo_server().await;
    let mut client = spawn_quiet(fx.client(
        &token,
        &["tcp", &echo.to_string(), "--remote-port", &public.to_string()],
    ));
    assert!(
        tcp_echo_works(public).await,
        "túnel deveria funcionar antes da revogação"
    );

    let out = fx.token_cmd(&["revoke", "vai-ser-revogado"]);
    assert!(out.status.success());

    // Em até ~1 heartbeat o servidor derruba a sessão; o cliente reconecta,
    // é recusado na autenticação (erro fatal) e SAI.
    let st = timeout(Duration::from_secs(30), client.wait())
        .await
        .expect("cliente deveria desistir após a revogação")
        .unwrap();
    assert!(!st.success());
    assert!(
        TcpStream::connect(("127.0.0.1", public)).await.is_err(),
        "porta pública deveria ter sido liberada"
    );
}

#[tokio::test]
async fn token_cli_create_list_revoke() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token(
        "notebook",
        &["--ports", "20000-20010", "--subdomains", "ana,ana-dev"],
    );
    let (id, secret) = token.split_once('.').unwrap();

    let list = String::from_utf8(fx.token_cmd(&["list"]).stdout).unwrap();
    assert!(list.contains("notebook") && list.contains(id) && list.contains("20000-20010"));
    assert!(list.contains("ana,ana-dev"));
    assert!(!list.contains(secret), "list não pode mostrar o segredo");
    let on_disk = std::fs::read_to_string(fx.dir.join("tokens.json")).unwrap();
    assert!(!on_disk.contains(secret), "o segredo cru não pode estar no disco");

    assert!(
        !fx.token_cmd(&["create", "notebook"]).status.success(),
        "nome duplicado"
    );
    assert!(fx.token_cmd(&["revoke", "notebook"]).status.success());
    assert!(!fx.token_cmd(&["revoke", "notebook"]).status.success());
    let list = String::from_utf8(fx.token_cmd(&["list"]).stdout).unwrap();
    assert!(list.contains("nenhum token"));
}

// ---------------------------------------------------------------- configure

#[tokio::test]
async fn configure_saves_credentials_so_no_flags_are_needed() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let echo = tcp_echo_server().await;

    // Diretório de config isolado: o teste nunca toca no ~/.config real.
    let home = fx.dir.join("home");
    let hbt = bin_dir().join("hbt");
    let ok = std::process::Command::new(&hbt)
        .args(["configure", "--server", &fx.addr(), "--token", &token])
        .env("XDG_CONFIG_HOME", &home)
        .env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER")
        .status()
        .unwrap()
        .success();
    assert!(ok);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("hbt/config.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "arquivo com segredo deve ser privado");
    }

    // Agora SEM --server, SEM --token e SEM env: tudo vem do arquivo.
    let mut c = cmd("hbt");
    c.args(["tcp", &echo.to_string(), "--remote-port", &public.to_string()])
        .env("XDG_CONFIG_HOME", &home);
    let (_client, url) = banner_url(c).await;
    assert_eq!(url, format!("tcp://127.0.0.1:{public}"));
}

fn isolated_config(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hbt-e2e-{tag}-{}", std::process::id()))
}

fn hbt_output(home: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(bin_dir().join("hbt"))
        .args(args)
        .env("XDG_CONFIG_HOME", home)
        .env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER")
        .output()
        .unwrap()
}

#[test]
fn client_without_configuration_explains_what_to_do() {
    let home = isolated_config("empty");
    let out = hbt_output(&home, &["http", "3000"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("hbt configure"));
}

#[test]
fn configure_rejects_malformed_tokens() {
    let home = isolated_config("badtoken");
    let out = hbt_output(&home, &["configure", "--server", "x", "--token", "sem-formato"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("<id>.<segredo>"));
    assert!(
        !home.join("hbt/config.toml").exists(),
        "não deve gravar token inválido"
    );
}
