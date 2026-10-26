//! Ferramenta de carga UDP para o teste em docker/compose.loadtest.yml.
//!
//!   hbt-udpbench echo --bind 0.0.0.0:9000            # devolve cada datagrama a quem mandou
//!   hbt-udpbench ping --target server:20001          # espera o túnel responder
//!   hbt-udpbench load --target server:20001 --rate 50000 --peers 10 --size 512 --duration 20s
//!
//! Cada pacote leva (número de sequência, instante de envio); o eco volta para o
//! mesmo socket e medimos perda e RTT. Cada peer é um socket próprio, ou seja,
//! uma origem distinta para o túnel (= uma sessão no cliente hbt).
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Servidor de eco UDP.
    Echo {
        #[arg(long, default_value = "0.0.0.0:9000")]
        bind: String,
        /// Tasks lendo o mesmo socket em paralelo.
        #[arg(long, default_value_t = 4)]
        workers: usize,
    },
    /// Manda um pacote a cada 200ms até ter resposta (ou estourar --timeout).
    Ping {
        #[arg(long)]
        target: String,
        #[arg(long, default_value = "15s", value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Gera carga a uma taxa fixa e mede perda e RTT.
    Load {
        #[arg(long)]
        target: String,
        /// Pacotes por segundo no total (divididos entre os peers).
        #[arg(long, default_value_t = 10_000)]
        rate: u64,
        #[arg(long, default_value_t = 10)]
        peers: usize,
        /// Tamanho do datagrama em bytes (mínimo 16).
        #[arg(long, default_value_t = 512)]
        size: usize,
        #[arg(long, default_value = "10s", value_parser = parse_duration)]
        duration: Duration,
    },
}

/// "20s", "500ms", "2m" ou só segundos ("20").
fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num.parse().map_err(|e| format!("{e}"))?;
    Ok(Duration::from_secs_f64(v * mult))
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Echo { bind, workers } => echo(&bind, workers).await,
        Cmd::Ping { target, timeout } => ping(&target, timeout).await,
        Cmd::Load {
            target,
            rate,
            peers,
            size,
            duration,
        } => load(&target, rate, peers, size.max(16), duration).await,
    }
}

async fn echo(bind: &str, workers: usize) -> Result<()> {
    let sock = Arc::new(UdpSocket::bind(bind).await?);
    eprintln!("eco UDP em {}", sock.local_addr()?);
    let mut tasks = Vec::new();
    for _ in 0..workers.max(1) {
        let sock = sock.clone();
        tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                if let Ok((n, peer)) = sock.recv_from(&mut buf).await {
                    let _ = sock.send_to(&buf[..n], peer).await;
                }
            }
        }));
    }
    for t in tasks {
        t.await?;
    }
    Ok(())
}

async fn connected_socket(target: SocketAddr) -> Result<UdpSocket> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.connect(target).await?;
    Ok(sock)
}

async fn resolve(target: &str) -> Result<SocketAddr> {
    tokio::net::lookup_host(target)
        .await?
        .next()
        .with_context(|| format!("não resolvi {target}"))
}

async fn ping(target: &str, limit: Duration) -> Result<()> {
    let sock = connected_socket(resolve(target).await?).await?;
    let deadline = Instant::now() + limit;
    let mut buf = [0u8; 64];
    while Instant::now() < deadline {
        let _ = sock.send(b"ping").await;
        if let Ok(Ok(_)) = timeout(Duration::from_millis(200), sock.recv(&mut buf)).await {
            return Ok(());
        }
    }
    bail!("{target} não respondeu em {limit:?}")
}

struct PeerResult {
    sent: u64,
    send_errors: u64,
    rtts_us: Vec<u32>,
}

async fn load(target: &str, rate: u64, peers: usize, size: usize, duration: Duration) -> Result<()> {
    let target = resolve(target).await?;
    let base = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let per_peer = rate as f64 / peers as f64;

    let mut handles = Vec::with_capacity(peers);
    for _ in 0..peers {
        let sock = Arc::new(connected_socket(target).await?);
        let stop = stop.clone();
        let rx = tokio::spawn(receiver(sock.clone(), base, stop.clone()));
        let tx = tokio::spawn(sender(sock, base, per_peer, size, duration));
        handles.push((tx, rx));
    }

    let mut total = PeerResult {
        sent: 0,
        send_errors: 0,
        rtts_us: Vec::new(),
    };
    for (tx, _) in &mut handles {
        let (sent, errs) = tx.await?;
        total.sent += sent;
        total.send_errors += errs;
    }
    // Margem para os últimos ecos chegarem; o que vier depois disso conta como perdido.
    sleep(Duration::from_secs(1)).await;
    stop.store(true, Ordering::Relaxed);
    for (_, rx) in handles {
        total.rtts_us.extend(rx.await?);
    }

    report(&total, rate, peers, size, duration);
    Ok(())
}

/// Envia `per_peer` pacotes/s. O timer do tokio tem resolução de ~1ms, então
/// a cada tick mandamos quantos pacotes "deviam" ter saído até agora.
async fn sender(
    sock: Arc<UdpSocket>,
    base: Instant,
    per_peer: f64,
    size: usize,
    duration: Duration,
) -> (u64, u64) {
    let start = Instant::now();
    let mut tick = interval(Duration::from_millis(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut buf = vec![0u8; size];
    let (mut seq, mut errors) = (0u64, 0u64);
    loop {
        tick.tick().await;
        let elapsed = start.elapsed();
        if elapsed >= duration {
            return (seq, errors);
        }
        let due = (elapsed.as_secs_f64() * per_peer) as u64;
        while seq < due {
            buf[..8].copy_from_slice(&seq.to_be_bytes());
            buf[8..16].copy_from_slice(&(base.elapsed().as_nanos() as u64).to_be_bytes());
            // ECONNREFUSED (ICMP de porta fechada) e ENOBUFS caem aqui.
            if sock.send(&buf).await.is_err() {
                errors += 1;
            }
            seq += 1;
        }
    }
}

async fn receiver(sock: Arc<UdpSocket>, base: Instant, stop: Arc<AtomicBool>) -> Vec<u32> {
    let mut rtts = Vec::new();
    let mut buf = vec![0u8; 65535];
    while !stop.load(Ordering::Relaxed) {
        match timeout(Duration::from_millis(100), sock.recv(&mut buf)).await {
            Ok(Ok(n)) if n >= 16 => {
                let sent_ns = u64::from_be_bytes(buf[8..16].try_into().unwrap());
                let rtt_ns = (base.elapsed().as_nanos() as u64).saturating_sub(sent_ns);
                rtts.push((rtt_ns / 1000).min(u32::MAX as u64) as u32);
            }
            _ => {}
        }
    }
    rtts
}

fn report(r: &PeerResult, rate: u64, peers: usize, size: usize, duration: Duration) {
    let secs = duration.as_secs_f64();
    let recv = r.rtts_us.len() as u64;
    let loss = if r.sent == 0 {
        100.0
    } else {
        100.0 * (1.0 - recv as f64 / r.sent as f64).max(0.0)
    };
    let mut rtts = r.rtts_us.clone();
    rtts.sort_unstable();
    let pct = |p: f64| -> f64 {
        if rtts.is_empty() {
            return f64::NAN;
        }
        rtts[((rtts.len() - 1) as f64 * p) as usize] as f64 / 1000.0
    };
    println!("  alvo:      {rate} pps, {peers} peers, {size} B, {secs:.0}s");
    println!(
        "  enviados:  {} ({:.0} pps, {:.1} Mbit/s){}",
        r.sent,
        r.sent as f64 / secs,
        r.sent as f64 * size as f64 * 8.0 / secs / 1e6,
        if r.send_errors > 0 {
            format!(", {} erros de envio", r.send_errors)
        } else {
            String::new()
        }
    );
    println!(
        "  recebidos: {recv} ({:.0} pps)  perda {loss:.2}%",
        recv as f64 / secs
    );
    println!(
        "  RTT:       p50 {:.2} ms  p99 {:.2} ms  máx {:.2} ms",
        pct(0.50),
        pct(0.99),
        pct(1.0)
    );
}
