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

#[tokio::test]
async fn tunnel_cap_is_per_token() {
    let (p1, p2, p3) = (free_port(), free_port(), free_port());
    let fx = Fixture::start(
        &format!("{}-{}", p1.min(p2).min(p3), p1.max(p2).max(p3)),
        &["--max-tunnels-per-token", "1"],
    );
    let (a, b) = (fx.token("a", &[]), fx.token("b", &[]));
    let echo = tcp_echo_server().await;
    let tcp = |t: &str, p: u16| fx.client(t, &["tcp", &echo.to_string(), "--remote-port", &p.to_string()]);

    let _first = spawn_quiet(tcp(&a, p1));
    assert!(tcp_echo_works(p1).await, "primeiro túnel nunca respondeu");

    // Mesmo token, segundo túnel: recusado, a porta não abre e o primeiro segue de pé.
    let _second = spawn_quiet(tcp(&a, p2));
    sleep(Duration::from_secs(2)).await;
    assert!(
        TcpStream::connect(("127.0.0.1", p2)).await.is_err(),
        "o segundo túnel passou do teto"
    );
    assert!(tcp_echo_works(p1).await, "o primeiro túnel caiu");

    // Outro token tem o próprio teto.
    let _other = spawn_quiet(tcp(&b, p3));
    assert!(tcp_echo_works(p3).await, "o teto vazou para outro token");
}
