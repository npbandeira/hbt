//! Testes de ponta a ponta: sobem os binários reais e passam tráfego de verdade.
//! Rode com: cargo test -p hbt-e2e

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

const TOKEN: &str = "segredo-de-teste";

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

fn spawn(bin: &str, args: &[&str], token: &str) -> Child {
    Command::new(bin_dir().join(bin))
        .args(args)
        .env("HBT_TOKEN", token)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true) // sem isso, falha de teste deixaria processos órfãos
        .spawn()
        .unwrap()
}

fn start_server(control: u16, range: &str) -> Child {
    let c = format!("127.0.0.1:{control}");
    spawn(
        "hbt-server",
        &[
            "--control-bind",
            &c,
            "--tunnel-ip",
            "127.0.0.1",
            "--port-range",
            range,
        ],
        TOKEN,
    )
}

fn start_client(control: u16, token: &str, kind: &str, local: u16, remote: u16) -> Child {
    let (s, l, r) = (
        format!("127.0.0.1:{control}"),
        local.to_string(),
        remote.to_string(),
    );
    spawn("hbt", &["--server", &s, kind, &l, "--remote-port", &r], token)
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

#[tokio::test]
async fn tcp_tunnel_echoes_multiple_connections() {
    let (control, public) = (free_port(), free_port());
    let _server = start_server(control, &format!("{public}-{public}"));
    let echo = tcp_echo_server().await;
    let _client = start_client(control, TOKEN, "tcp", echo, public);

    let addr: SocketAddr = format!("127.0.0.1:{public}").parse().unwrap();
    // Servidor/cliente levam um instante para subir: tenta por até 10s.
    let mut ok = false;
    for _ in 0..50 {
        if let Ok(mut s) = TcpStream::connect(addr).await {
            if s.write_all(b"ping").await.is_ok() {
                let mut buf = [0u8; 4];
                if let Ok(Ok(_)) = timeout(Duration::from_millis(500), s.read_exact(&mut buf)).await {
                    assert_eq!(&buf, b"ping");
                    ok = true;
                    break;
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    assert!(ok, "túnel TCP nunca respondeu");

    // Várias conexões simultâneas, cada uma com seu payload.
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
    let (control, public) = (free_port(), free_port());
    let _server = start_server(control, &format!("{public}-{public}"));
    let echo = udp_echo_server().await;
    let _client = start_client(control, TOKEN, "udp", echo, public);

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
async fn wrong_token_makes_client_exit_with_error() {
    let (control, public) = (free_port(), free_port());
    let _server = start_server(control, &format!("{public}-{public}"));
    let mut client = start_client(control, "token-errado", "tcp", 1, public);
    let status = timeout(Duration::from_secs(10), client.wait())
        .await
        .expect("cliente deveria sair")
        .unwrap();
    assert!(!status.success());
}

#[tokio::test]
async fn port_outside_range_is_refused() {
    let (control, allowed, denied) = (free_port(), free_port(), free_port());
    let _server = start_server(control, &format!("{allowed}-{allowed}"));
    let _client = start_client(control, TOKEN, "tcp", 1, denied);
    sleep(Duration::from_millis(1500)).await;
    assert!(
        TcpStream::connect(("127.0.0.1", denied)).await.is_err(),
        "porta fora da faixa não deveria abrir"
    );
}

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

/// Faz um GET cru para o edge HTTP com o `Host` dado e devolve a resposta inteira.
async fn http_get(edge: u16, host: &str) -> String {
    // Servidor ainda subindo: devolve vazio e quem chama tenta de novo.
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", edge)).await else {
        return String::new();
    };
    let req = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    timeout(Duration::from_secs(5), s.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    out
}

#[tokio::test]
async fn http_tunnel_routes_by_host_header() {
    let (control, public, edge) = (free_port(), free_port(), free_port());
    let (c, r, h) = (
        format!("127.0.0.1:{control}"),
        format!("{public}-{public}"),
        format!("127.0.0.1:{edge}"),
    );
    let _server = spawn(
        "hbt-server",
        &[
            "--control-bind",
            &c,
            "--tunnel-ip",
            "127.0.0.1",
            "--port-range",
            &r,
            "--domain",
            "localhost",
            "--http-bind",
            &h,
        ],
        TOKEN,
    );
    let web = http_ok_server().await;
    let web_s = web.to_string();
    let _client = spawn(
        "hbt",
        &["--server", &c, "http", &web_s, "--subdomain", "demo"],
        TOKEN,
    );

    // Espera o túnel ficar de pé (com e sem :porta no Host, ambos devem rotear).
    let host = format!("demo.localhost:{edge}");
    let mut resp = String::new();
    for _ in 0..50 {
        resp = http_get(edge, &host).await;
        if resp.contains("200 OK") {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    assert!(
        resp.starts_with("HTTP/1.1 200 OK") && resp.ends_with("ok"),
        "resposta: {resp:?}"
    );
    assert!(
        http_get(edge, "DEMO.localhost").await.contains("200 OK"),
        "Host deve ser case-insensitive"
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
    let (control, public) = (free_port(), free_port());
    let _server = start_server(control, &format!("{public}-{public}"));
    let c = format!("127.0.0.1:{control}");
    let mut client = spawn("hbt", &["--server", &c, "http", "1"], TOKEN);
    // Recusa é não-fatal (o cliente tenta de novo), então ele NÃO deve sair sozinho.
    assert!(timeout(Duration::from_secs(2), client.wait()).await.is_err());
}

/// Sobe o cliente com stdout capturado e devolve a URL do banner "Forwarding".
async fn banner_url(cmd: &mut Command) -> (Child, String) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
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

#[tokio::test]
async fn server_picks_a_random_port_when_none_requested() {
    let (control, lo) = (free_port(), free_port());
    let hi = lo.saturating_add(20);
    let _server = start_server(control, &format!("{lo}-{hi}"));
    sleep(Duration::from_millis(500)).await;
    let echo = tcp_echo_server().await;

    let (c, e) = (format!("127.0.0.1:{control}"), echo.to_string());
    let mut cmd = Command::new(bin_dir().join("hbt"));
    cmd.args(["--server", &c, "tcp", &e]).env("HBT_TOKEN", TOKEN);
    let (_client, url) = banner_url(&mut cmd).await;

    let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
    assert!((lo..=hi).contains(&port), "porta {port} fora de {lo}-{hi}");
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(b"oi").await.unwrap();
    let mut buf = [0u8; 2];
    timeout(Duration::from_secs(5), s.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"oi");
}

#[tokio::test]
async fn configure_saves_credentials_so_no_flags_are_needed() {
    let (control, public) = (free_port(), free_port());
    let _server = start_server(control, &format!("{public}-{public}"));
    let echo = tcp_echo_server().await;

    // Diretório de config isolado: o teste nunca toca no ~/.config real.
    let home = std::env::temp_dir().join(format!("hbt-e2e-{}-{control}", std::process::id()));
    let hbt = bin_dir().join("hbt");
    let c = format!("127.0.0.1:{control}");
    let ok = std::process::Command::new(&hbt)
        .args(["configure", "--server", &c, "--token", TOKEN])
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
    let (e, p) = (echo.to_string(), public.to_string());
    let mut cmd = Command::new(&hbt);
    cmd.args(["tcp", &e, "--remote-port", &p])
        .env("XDG_CONFIG_HOME", &home)
        .env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER");
    let (_client, url) = banner_url(&mut cmd).await;
    assert_eq!(url, format!("tcp://127.0.0.1:{public}"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn client_without_configuration_explains_what_to_do() {
    let home = std::env::temp_dir().join(format!("hbt-e2e-empty-{}", std::process::id()));
    let out = std::process::Command::new(bin_dir().join("hbt"))
        .args(["http", "3000"])
        .env("XDG_CONFIG_HOME", &home)
        .env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("hbt configure"));
}
