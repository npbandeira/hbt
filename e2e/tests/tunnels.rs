//! Túneis TCP/UDP: tráfego, multiplexação e portas.

mod common;

use common::*;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::{sleep, timeout};

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

/// Sockets TCP (fora o LISTEN) com `port` numa das pontas, em qualquer
/// estado: estabelecidos, fechando ou em TIME_WAIT, de qualquer dos lados.
fn tcp_sockets_on(port: u16) -> usize {
    let table = std::fs::read_to_string("/proc/net/tcp").unwrap_or_default();
    let port_of = |addr: &str| u16::from_str_radix(addr.rsplit(':').next().unwrap(), 16).ok();
    table
        .lines()
        .skip(1)
        .filter(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let on_port = port_of(f[1]) == Some(port) || port_of(f[2]) == Some(port);
            on_port && f[3] != "0A" // 0A = LISTEN
        })
        .count()
}

#[tokio::test]
async fn many_concurrent_connections_share_one_client_connection() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let echo = tcp_echo_server().await;
    let _client = spawn_quiet(fx.client(
        &token,
        &["tcp", &echo.to_string(), "--remote-port", &public.to_string()],
    ));
    assert!(tcp_echo_works(public).await, "túnel TCP nunca respondeu");

    // 300 usuários ao mesmo tempo, cada um com conexão própria e curta.
    let addr: SocketAddr = format!("127.0.0.1:{public}").parse().unwrap();
    let mut tasks = vec![];
    for i in 0..300u32 {
        tasks.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            let msg = format!("usuário {i}").into_bytes();
            s.write_all(&msg).await.unwrap();
            let mut got = vec![0u8; msg.len()];
            s.read_exact(&mut got).await.unwrap();
            assert_eq!(got, msg);
        }));
    }
    for t in tasks {
        timeout(Duration::from_secs(20), t).await.unwrap().unwrap();
    }

    // O ponto da multiplexação: na porta de controle existe só a conexão da
    // sessão (2 sockets: o lado do cliente e o do servidor). No protocolo
    // antigo haveria ~300 conexões de dados a mais, ou seus restos em TIME_WAIT.
    if cfg!(target_os = "linux") {
        assert_eq!(tcp_sockets_on(fx.control), 2);
    }
}

#[tokio::test]
async fn old_protocol_client_is_told_to_upgrade() {
    use hbt_proto::{read_msg, write_msg, ClientMsg, Hello, ServerMsg};
    let fx = Fixture::start("40000-40000", &[]);
    let mut s = None;
    for _ in 0..50 {
        if let Ok(c) = TcpStream::connect(fx.addr()).await {
            s = Some(c);
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let mut s = s.expect("servidor não subiu");
    // Exatamente o que o cliente do protocolo 1 manda.
    write_msg(
        &mut s,
        &Hello::V1Control {
            token_id: "abc".into(),
        },
    )
    .await
    .unwrap();
    let ServerMsg::Challenge(_) = read_msg(&mut s).await.unwrap() else {
        panic!("esperava Challenge");
    };
    write_msg(&mut s, &ClientMsg::Auth(vec![0; 32])).await.unwrap();
    match read_msg(&mut s).await.unwrap() {
        ServerMsg::Error(e) => assert!(e.contains("atualize"), "{e}"),
        other => panic!("esperava Error, veio {other:?}"),
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
