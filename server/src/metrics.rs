//! Métricas no formato texto do Prometheus, servidas em `GET /metrics`.
//!
//! Feito à mão com atômicos em vez de uma crate: são poucas séries, com rótulos
//! fixos (tipo de túnel, sentido), então não precisamos de registro dinâmico.
//! Cada contador é um `AtomicU64` com `Ordering::Relaxed`: só queremos que o
//! incremento não se perca; não há outra memória a sincronizar com ele.
//!
//! Sentido dos bytes/datagramas: `in` = vindo do usuário público (rumo ao
//! serviço do cliente), `out` = resposta voltando para o usuário público.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tracing::{debug, warn};

#[derive(Clone, Copy)]
pub enum Kind {
    Tcp = 0,
    Udp = 1,
    Http = 2,
}

const KINDS: [&str; 3] = ["tcp", "udp", "http"];

#[derive(Clone, Copy)]
pub enum ConnError {
    /// O cliente não abriu a conexão de dados em 10s.
    DataTimeout = 0,
    /// HTTP para um subdomínio sem túnel (404).
    NoRoute = 1,
    /// O túnel existia mas o canal de controle já tinha caído (502).
    TunnelOffline = 2,
    /// Requisição HTTP inválida ou sem Host (400).
    BadRequest = 3,
}

const CONN_ERRORS: [&str; 4] = ["data_timeout", "no_route", "tunnel_offline", "bad_request"];

/// Limites (em segundos) do histograma de espera pela conexão de dados: o
/// tempo entre pedir `NewConn` ao cliente e ele conectar de volta. É ~1 RTT
/// servidor<->cliente; é o custo que multiplexar conexões eliminaria.
const WAIT_BUCKETS: [f64; 13] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Default)]
pub struct Metrics {
    sessions_active: [AtomicI64; 3],
    sessions_total: [AtomicU64; 3],
    handshake_failures: AtomicU64,
    auth_failures: AtomicU64,
    conns_active: [AtomicI64; 3],
    conns_total: [AtomicU64; 3],
    conn_errors: [AtomicU64; 4],
    /// [tipo][0 = in, 1 = out]
    bytes: [[AtomicU64; 2]; 3],
    udp_datagrams: [AtomicU64; 2],
    wait_buckets: [AtomicU64; WAIT_BUCKETS.len()],
    wait_count: AtomicU64,
    wait_sum_us: AtomicU64,
}

/// Mantém um gauge "ativo" incrementado enquanto viver (RAII).
pub struct ActiveGuard<'a>(&'a AtomicI64);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

impl Metrics {
    pub fn session(&self, kind: Kind) -> ActiveGuard<'_> {
        self.sessions_total[kind as usize].fetch_add(1, Relaxed);
        self.sessions_active[kind as usize].fetch_add(1, Relaxed);
        ActiveGuard(&self.sessions_active[kind as usize])
    }

    pub fn conn(&self, kind: Kind) -> ActiveGuard<'_> {
        self.conns_total[kind as usize].fetch_add(1, Relaxed);
        self.conns_active[kind as usize].fetch_add(1, Relaxed);
        ActiveGuard(&self.conns_active[kind as usize])
    }

    pub fn handshake_failed(&self) {
        self.handshake_failures.fetch_add(1, Relaxed);
    }

    pub fn auth_failed(&self) {
        self.auth_failures.fetch_add(1, Relaxed);
    }

    pub fn conn_error(&self, e: ConnError) {
        self.conn_errors[e as usize].fetch_add(1, Relaxed);
    }

    pub fn bytes(&self, kind: Kind, inbound: u64, outbound: u64) {
        self.bytes[kind as usize][0].fetch_add(inbound, Relaxed);
        self.bytes[kind as usize][1].fetch_add(outbound, Relaxed);
    }

    pub fn udp_in(&self, len: usize) {
        self.udp_datagrams[0].fetch_add(1, Relaxed);
        self.bytes[Kind::Udp as usize][0].fetch_add(len as u64, Relaxed);
    }

    pub fn udp_out(&self, len: usize) {
        self.udp_datagrams[1].fetch_add(1, Relaxed);
        self.bytes[Kind::Udp as usize][1].fetch_add(len as u64, Relaxed);
    }

    pub fn data_wait(&self, d: Duration) {
        let secs = d.as_secs_f64();
        // Guardamos cada amostra só no primeiro balde que a contém; o formato
        // do Prometheus quer baldes cumulativos, somados na hora de exportar.
        if let Some(i) = WAIT_BUCKETS.iter().position(|&b| secs <= b) {
            self.wait_buckets[i].fetch_add(1, Relaxed);
        }
        self.wait_count.fetch_add(1, Relaxed);
        self.wait_sum_us.fetch_add(d.as_micros() as u64, Relaxed);
    }

    pub fn render(&self, started: SystemTime) -> String {
        let mut o = String::with_capacity(4096);
        let g = |a: &AtomicI64| a.load(Relaxed);
        let c = |a: &AtomicU64| a.load(Relaxed);

        header(&mut o, "hbt_build_info", "gauge", "Versão do hbt-server.");
        let _ = writeln!(o, "hbt_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"));

        header(
            &mut o,
            "hbt_sessions_active",
            "gauge",
            "Clientes conectados (um por túnel).",
        );
        for (i, k) in KINDS.iter().enumerate() {
            let _ = writeln!(
                o,
                "hbt_sessions_active{{kind=\"{k}\"}} {}",
                g(&self.sessions_active[i])
            );
        }
        header(
            &mut o,
            "hbt_sessions_total",
            "counter",
            "Túneis abertos desde o início.",
        );
        for (i, k) in KINDS.iter().enumerate() {
            let _ = writeln!(
                o,
                "hbt_sessions_total{{kind=\"{k}\"}} {}",
                c(&self.sessions_total[i])
            );
        }
        header(
            &mut o,
            "hbt_handshake_failures_total",
            "counter",
            "Clientes recusados no handshake (inclui autenticação, porta/subdomínio negado, timeout).",
        );
        let _ = writeln!(o, "hbt_handshake_failures_total {}", c(&self.handshake_failures));
        header(
            &mut o,
            "hbt_auth_failures_total",
            "counter",
            "Tokens inválidos ou revogados.",
        );
        let _ = writeln!(o, "hbt_auth_failures_total {}", c(&self.auth_failures));

        // UDP não tem "conexões" (um stream só por túnel), então fica de fora.
        header(
            &mut o,
            "hbt_connections_active",
            "gauge",
            "Conexões públicas abertas agora.",
        );
        for k in [Kind::Tcp, Kind::Http] {
            let _ = writeln!(
                o,
                "hbt_connections_active{{kind=\"{}\"}} {}",
                KINDS[k as usize],
                g(&self.conns_active[k as usize])
            );
        }
        header(
            &mut o,
            "hbt_connections_total",
            "counter",
            "Conexões públicas aceitas.",
        );
        for k in [Kind::Tcp, Kind::Http] {
            let _ = writeln!(
                o,
                "hbt_connections_total{{kind=\"{}\"}} {}",
                KINDS[k as usize],
                c(&self.conns_total[k as usize])
            );
        }
        header(
            &mut o,
            "hbt_connection_errors_total",
            "counter",
            "Conexões públicas que não chegaram ao serviço.",
        );
        for (i, r) in CONN_ERRORS.iter().enumerate() {
            let _ = writeln!(
                o,
                "hbt_connection_errors_total{{reason=\"{r}\"}} {}",
                c(&self.conn_errors[i])
            );
        }

        header(&mut o, "hbt_bytes_total", "counter",
            "Bytes trafegados (in = do usuário público, out = para ele). TCP/HTTP contam ao fechar a conexão.");
        for (i, k) in KINDS.iter().enumerate() {
            for (j, dir) in ["in", "out"].iter().enumerate() {
                let _ = writeln!(
                    o,
                    "hbt_bytes_total{{kind=\"{k}\",direction=\"{dir}\"}} {}",
                    c(&self.bytes[i][j])
                );
            }
        }
        header(
            &mut o,
            "hbt_udp_datagrams_total",
            "counter",
            "Datagramas UDP repassados.",
        );
        for (j, dir) in ["in", "out"].iter().enumerate() {
            let _ = writeln!(
                o,
                "hbt_udp_datagrams_total{{direction=\"{dir}\"}} {}",
                c(&self.udp_datagrams[j])
            );
        }

        header(
            &mut o,
            "hbt_data_conn_wait_seconds",
            "histogram",
            "Espera entre pedir a conexão de dados ao cliente e ela chegar (~1 RTT).",
        );
        let mut acc = 0;
        for (i, b) in WAIT_BUCKETS.iter().enumerate() {
            acc += c(&self.wait_buckets[i]);
            let _ = writeln!(o, "hbt_data_conn_wait_seconds_bucket{{le=\"{b}\"}} {acc}");
        }
        let count = c(&self.wait_count);
        let _ = writeln!(o, "hbt_data_conn_wait_seconds_bucket{{le=\"+Inf\"}} {count}");
        let _ = writeln!(
            o,
            "hbt_data_conn_wait_seconds_sum {}",
            c(&self.wait_sum_us) as f64 / 1e6
        );
        let _ = writeln!(o, "hbt_data_conn_wait_seconds_count {count}");

        process_metrics(&mut o, started);
        o
    }
}

fn header(o: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

/// Métricas padrão `process_*` lidas de /proc (só Linux; fora dele, omitidas).
/// Sem libc: assumimos USER_HZ = 100 e páginas de 4 KiB, os valores do Linux
/// em x86_64 e aarch64 comuns.
fn process_metrics(o: &mut String, started: SystemTime) {
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        // O nome do processo (campo 2) pode ter espaços: os campos seguem o ')'.
        if let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) {
            let f: Vec<&str> = rest.split_whitespace().collect();
            // utime e stime são os campos 14 e 15 => índices 11 e 12 após o ')'.
            let ticks: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
                + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            header(
                o,
                "process_cpu_seconds_total",
                "counter",
                "CPU usada (usuário + sistema).",
            );
            let _ = writeln!(o, "process_cpu_seconds_total {}", ticks as f64 / 100.0);
        }
    }
    if let Some(rss) = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
    {
        header(
            o,
            "process_resident_memory_bytes",
            "gauge",
            "Memória residente (RSS).",
        );
        let _ = writeln!(o, "process_resident_memory_bytes {}", rss * 4096);
    }
    if let Ok(fds) = std::fs::read_dir("/proc/self/fd") {
        header(
            o,
            "process_open_fds",
            "gauge",
            "Descritores de arquivo abertos (sockets incluídos).",
        );
        let _ = writeln!(o, "process_open_fds {}", fds.count());
    }
    if let Ok(t) = started.duration_since(UNIX_EPOCH) {
        header(
            o,
            "process_start_time_seconds",
            "gauge",
            "Início do processo (unix).",
        );
        let _ = writeln!(o, "process_start_time_seconds {}", t.as_secs());
    }
}

/// Servidor HTTP mínimo: qualquer `GET` recebe as métricas.
pub async fn serve(listener: TcpListener, metrics: Arc<Metrics>) {
    let started = SystemTime::now();
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("accept de métricas falhou: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            if let Err(e) = respond(stream, &metrics, started).await {
                debug!("métricas {peer}: {e:#}");
            }
        });
    }
}

async fn respond(mut s: TcpStream, metrics: &Metrics, started: SystemTime) -> Result<()> {
    // Lemos só o começo do pedido: o caminho não importa e não há corpo.
    let mut buf = [0u8; 1024];
    let n = timeout(Duration::from_secs(5), s.read(&mut buf))
        .await
        .context("timeout lendo pedido")??;
    let (status, body) = if buf[..n].starts_with(b"GET ") {
        ("200 OK", metrics.render(started))
    } else {
        ("405 Method Not Allowed", String::from("use GET\n"))
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(body.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_is_cumulative_and_gauges_follow_guards() {
        let m = Metrics::default();
        m.data_wait(Duration::from_micros(800)); // <= 1ms
        m.data_wait(Duration::from_millis(70)); // <= 100ms
        m.data_wait(Duration::from_secs(30)); // só em +Inf
        let conn = m.conn(Kind::Http);
        m.bytes(Kind::Tcp, 10, 20);

        let out = m.render(SystemTime::now());
        assert!(out.contains("hbt_data_conn_wait_seconds_bucket{le=\"0.001\"} 1\n"));
        assert!(out.contains("hbt_data_conn_wait_seconds_bucket{le=\"0.05\"} 1\n"));
        assert!(out.contains("hbt_data_conn_wait_seconds_bucket{le=\"0.1\"} 2\n"));
        assert!(out.contains("hbt_data_conn_wait_seconds_bucket{le=\"10\"} 2\n"));
        assert!(out.contains("hbt_data_conn_wait_seconds_bucket{le=\"+Inf\"} 3\n"));
        assert!(out.contains("hbt_data_conn_wait_seconds_count 3\n"));
        assert!(out.contains("hbt_connections_active{kind=\"http\"} 1\n"));
        assert!(out.contains("hbt_bytes_total{kind=\"tcp\",direction=\"out\"} 20\n"));

        drop(conn);
        let out = m.render(SystemTime::now());
        assert!(out.contains("hbt_connections_active{kind=\"http\"} 0\n"));
        assert!(out.contains("hbt_connections_total{kind=\"http\"} 1\n"));
    }
}
