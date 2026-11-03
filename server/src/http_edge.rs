//! Edge HTTP público: roteia pelo `Host` para o túnel do subdomínio.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::metrics::{ConnError, Kind};
use crate::state::State;
use crate::tunnel::open_conn;

/// Tamanho máximo da linha de requisição + cabeçalhos que aceitamos ler.
const MAX_HEAD: usize = 16 * 1024;

pub async fn http_edge(listener: TcpListener, state: Arc<State>) {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::db::Store;
    use crate::state::HttpCfg;

    fn state(domain: Option<&str>) -> State {
        State {
            store: Store::memory().unwrap(),
            tunnel_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port_range: 20000..=30000,
            http: domain.map(|d| HttpCfg {
                domain: d.to_owned(),
                port: 80,
            }),
            routes: Mutex::new(HashMap::new()),
            metrics: Arc::default(),
            online: Mutex::default(),
            max_tunnels: 2,
        }
    }

    #[test]
    fn extracts_single_level_subdomain() {
        let st = state(Some("tunnel.exemplo.com"));
        assert_eq!(
            subdomain_of(&st, "app.tunnel.exemplo.com").as_deref(),
            Some("app")
        );
        assert_eq!(
            subdomain_of(&st, "App.Tunnel.Exemplo.com:8080").as_deref(),
            Some("app")
        );
    }

    #[test]
    fn rejects_hosts_outside_the_domain() {
        let st = state(Some("tunnel.exemplo.com"));
        assert_eq!(subdomain_of(&st, "tunnel.exemplo.com"), None);
        assert_eq!(subdomain_of(&st, "a.b.tunnel.exemplo.com"), None);
        assert_eq!(subdomain_of(&st, "app.outro.com"), None);
        assert_eq!(subdomain_of(&st, "apptunnel.exemplo.com"), None);
    }

    #[test]
    fn http_disabled_means_no_routing() {
        assert_eq!(subdomain_of(&state(None), "app.tunnel.exemplo.com"), None);
    }
}
