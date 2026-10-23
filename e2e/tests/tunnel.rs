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
        format!("127.0.0.1:{local}"),
        remote.to_string(),
    );
    spawn(
        "hbt",
        &["--server", &s, kind, "--local", &l, "--remote-port", &r],
        token,
    )
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
