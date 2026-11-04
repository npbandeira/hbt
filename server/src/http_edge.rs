//! Edge HTTP público: roteia pelo `Host` para o túnel do subdomínio.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{copy_bidirectional, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, warn};

use crate::metrics::{ConnError, Kind};
use crate::state::State;
use crate::tunnel::open_conn;

/// Tamanho máximo da linha de requisição + cabeçalhos que aceitamos ler.
const MAX_HEAD: usize = 16 * 1024;
/// Prazo para o handshake TLS e para os cabeçalhos chegarem.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

async fn accept_loop<F, Fut>(listener: TcpListener, what: &'static str, handle: F)
where
    F: Fn(TcpStream, SocketAddr) -> Fut,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("accept {what} falhou: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Sem isso, Nagle + delayed ACK seguram respostas pequenas em keep-alive (~40ms).
        let _ = stream.set_nodelay(true);
        let fut = handle(stream, peer);
        tokio::spawn(async move {
            if let Err(e) = fut.await {
                debug!("{what} {peer}: {e:#}");
            }
        });
    }
}

/// Porta 80. Com certificado HTTPS pronto, só redireciona para https (308,
/// que preserva método e corpo); antes disso, serve os túneis em http.
pub async fn http_edge(listener: TcpListener, state: Arc<State>) {
    accept_loop(listener, "http", move |stream, _| {
        handle_http(stream, state.clone(), true)
    })
    .await;
}

/// Porta 443: termina o TLS. O domínio raiz é o site (se ligado); os
/// subdomínios são os túneis, que recebem HTTP puro como no edge da porta 80.
pub async fn https_edge(listener: TcpListener, state: Arc<State>, site: Option<axum::Router>) {
    let cfg = state
        .http
        .as_ref()
        .and_then(|h| h.https.as_ref())
        .expect("https_edge sem HttpsCfg");
    let acceptor: TlsAcceptor = cfg.certs.acceptor();
    let site = site.map(Arc::new);
    accept_loop(listener, "https", move |stream, peer| {
        let (state, acceptor, site) = (state.clone(), acceptor.clone(), site.clone());
        async move {
            let tls = timeout(HEAD_TIMEOUT, acceptor.accept(stream))
                .await
                .context("timeout no handshake TLS")?
                .context("handshake TLS")?;
            let sni = tls.get_ref().1.server_name().map(str::to_ascii_lowercase);
            let domain = &state.http.as_ref().unwrap().domain;
            match site {
                Some(site) if sni.as_deref() == Some(domain.as_str()) => {
                    crate::web::serve_conn(tls, peer, (*site).clone()).await;
                    Ok(())
                }
                _ => handle_http(tls, state, false).await,
            }
        }
    })
    .await;
}

/// Lê os cabeçalhos para descobrir o `Host`, escolhe o túnel e depois vira
/// um simples "cano" TCP: reenviamos os bytes já lidos e copiamos o resto.
///
/// Trade-off: roteamos por CONEXÃO, olhando só a primeira requisição. Não
/// reescrevemos nem inspecionamos requisições seguintes (keep-alive). Navegadores
/// usam uma conexão por origem, então funciona; um proxy que misture vários
/// hosts numa conexão só seria roteado pelo primeiro. Em troca, WebSocket,
/// streaming e uploads grandes funcionam sem esforço (não parseamos corpos).
async fn handle_http<S>(mut public: S, state: Arc<State>, plain: bool) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _conn = state.metrics.conn(Kind::Http);
    let (head, host, path) = match timeout(HEAD_TIMEOUT, read_http_head(&mut public)).await {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => {
            state.metrics.conn_error(ConnError::BadRequest);
            respond(&mut public, "400 Bad Request", "requisição HTTP inválida", None).await;
            return Err(e);
        }
        Err(_) => bail!("timeout lendo cabeçalhos"),
    };

    if plain {
        let http = state.http.as_ref();
        let name = strip_port(&host.to_ascii_lowercase()).to_owned();
        // Só para nomes nossos: não vira redirecionador para qualquer Host.
        let ours = http.is_some_and(|h| name == h.domain || name.ends_with(&format!(".{}", h.domain)));
        if let Some(port) = http.and_then(|h| h.https_port()).filter(|_| ours) {
            // Na 443 é definitivo (308, o navegador memoriza). Em outra porta é
            // quase sempre um teste lado a lado (p.ex. 8443 antes de trocar o
            // Caddy): 307, para não prender visitantes nela depois da troca.
            let (status, port) = if port == 443 {
                ("308 Permanent Redirect", String::new())
            } else {
                ("307 Temporary Redirect", format!(":{port}"))
            };
            let to = format!("https://{name}{port}{path}");
            respond(&mut public, status, &to, Some(&to)).await;
            return Ok(());
        }
    }

    let route = subdomain_of(&state, &host)
        .and_then(|sub| state.routes.lock().unwrap().get(&sub).map(|r| r.opener.clone()));
    let Some(route) = route else {
        state.metrics.conn_error(ConnError::NoRoute);
        respond(
            &mut public,
            "404 Not Found",
            &format!("nenhum túnel para '{host}'"),
            None,
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
        Err(ConnError::TunnelOffline) => respond(&mut public, "502 Bad Gateway", "túnel offline", None).await,
        Err(_) => {
            respond(
                &mut public,
                "504 Gateway Timeout",
                "o túnel está no limite de conexões simultâneas",
                None,
            )
            .await
        }
    }
    Ok(())
}

/// Tira ":porta" (só se o que vem depois dos ':' for número).
fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((n, port)) if port.bytes().all(|b| b.is_ascii_digit()) => n,
        _ => host,
    }
}

/// `Abc.exemplo.com:8080` -> `abc` (se o domínio base bater e for 1 nível só).
fn subdomain_of(state: &State, host: &str) -> Option<String> {
    let cfg = state.http.as_ref()?;
    let host = host.to_ascii_lowercase();
    let sub = strip_port(&host).strip_suffix(&format!(".{}", cfg.domain))?;
    (!sub.contains('.')).then(|| sub.to_owned())
}

/// Acumula bytes até os cabeçalhos HTTP/1.x estarem completos e devolve
/// (bytes lidos, valor do Host, caminho). `httparse` é o mesmo parser usado pelo
/// hyper: parsear HTTP à mão é fonte clássica de bugs de segurança (request smuggling).
async fn read_http_head<S: AsyncRead + Unpin>(s: &mut S) -> Result<(Vec<u8>, String, String)> {
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
                    .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_graphic()))
                    .map(str::to_owned)
                    .context("sem cabeçalho Host")?;
                let path = req.path.filter(|p| p.starts_with('/')).unwrap_or("/").to_owned();
                return Ok((buf, host, path));
            }
            httparse::Status::Partial if buf.len() >= MAX_HEAD => bail!("cabeçalhos grandes demais"),
            httparse::Status::Partial => {}
        }
    }
}

async fn respond<S: AsyncWrite + Unpin>(s: &mut S, status: &str, body: &str, location: Option<&str>) {
    let location = location.map(|l| format!("Location: {l}\r\n")).unwrap_or_default();
    let msg = format!(
        "HTTP/1.1 {status}\r\n{location}Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}\n",
        body.len() + 1
    );
    let _ = s.write_all(msg.as_bytes()).await;
    let _ = s.shutdown().await;
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
                https: None,
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
