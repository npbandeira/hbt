//! Sessões, cookies, limite de tentativas e checagens contra CSRF.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::Semaphore;

use crate::accounts;
use crate::db::UserRecord;
use crate::state::State;

use super::WebCfg;

const COOKIE: &str = "hbt_session";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

struct Session {
    user_id: String,
    expires: Instant,
}

/// Estado do site: sessões em memória (reiniciar o servidor desloga todos),
/// limite de tentativas por IP e o semáforo do argon2.
pub struct Web {
    pub state: Arc<State>,
    pub cfg: WebCfg,
    pub limiter: Limiter,
    sessions: Mutex<HashMap<String, Session>>,
    /// O argon2 usa ~19 MiB e algumas dezenas de ms de CPU por senha. Duas ao
    /// mesmo tempo no máximo: uma rajada de logins não trava os túneis nem
    /// estoura a RAM de um VPS pequeno.
    hashing: Semaphore,
}

impl Web {
    pub fn new(state: Arc<State>, cfg: WebCfg) -> Web {
        Web {
            state,
            cfg,
            limiter: Limiter::default(),
            sessions: Mutex::default(),
            hashing: Semaphore::new(2),
        }
    }

    pub async fn hash(&self, password: String) -> Result<String> {
        let _permit = self.hashing.acquire().await?;
        tokio::task::spawn_blocking(move || accounts::hash(&password)).await?
    }

    pub async fn verify(&self, hash: String, password: String) -> bool {
        let Ok(_permit) = self.hashing.acquire().await else {
            return false;
        };
        tokio::task::spawn_blocking(move || accounts::verify(&hash, &password))
            .await
            .unwrap_or(false)
    }

    /// Abre a sessão e redireciona para o painel.
    pub fn start_session(&self, user: &UserRecord, headers: &HeaderMap) -> Response {
        let sid = hbt_proto::hex_encode(&rand::random::<[u8; 32]>()); // 256 bits
        {
            let mut sessions = self.sessions.lock().unwrap();
            let now = Instant::now();
            if sessions.len() > 1000 {
                sessions.retain(|_, s| s.expires > now);
            }
            sessions.insert(
                sid.clone(),
                Session {
                    user_id: user.id.clone(),
                    expires: now + SESSION_TTL,
                },
            );
        }
        let cookie = format!(
            "{COOKIE}={sid}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
            SESSION_TTL.as_secs(),
            self.secure_attr(headers)
        );
        redirect("/dashboard", &cookie)
    }

    pub fn end_session(&self, headers: &HeaderMap) -> Response {
        if let Some(sid) = cookie_value(headers, COOKIE) {
            self.sessions.lock().unwrap().remove(&sid);
        }
        let cookie = format!(
            "{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
            self.secure_attr(headers)
        );
        redirect("/", &cookie)
    }

    /// A conta logada, se a sessão for válida **e a conta não estiver
    /// bloqueada**. A conta é lida do banco a cada requisição: o
    /// `hbt-server user disable` roda noutro processo e vale na hora.
    pub async fn current_user(&self, headers: &HeaderMap) -> Option<UserRecord> {
        let sid = cookie_value(headers, COOKIE)?;
        let user_id = {
            let mut sessions = self.sessions.lock().unwrap();
            match sessions.get(&sid) {
                Some(s) if s.expires > Instant::now() => s.user_id.clone(),
                Some(_) => {
                    sessions.remove(&sid);
                    return None;
                }
                None => return None,
            }
        };
        let user = self
            .state
            .store
            .run(move |s| s.user_by_id(&user_id))
            .await
            .ok()??;
        (!user.disabled).then_some(user)
    }

    /// `host:porta` do servidor de túneis, para o comando de configuração:
    /// o host do `--web-url` (ou do cabeçalho `Host`) e a porta do QUIC.
    pub fn server_addr(&self, headers: &HeaderMap) -> String {
        let host = self
            .cfg
            .url
            .as_deref()
            .map(host_of_url)
            .or_else(|| {
                headers
                    .get(header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(host_of_authority)
            })
            .unwrap_or("SEU_SERVIDOR");
        format!("{host}:{}", self.cfg.control_port)
    }

    fn secure_attr(&self, headers: &HeaderMap) -> &'static str {
        let https = headers.get("x-forwarded-proto").is_some_and(|v| v == "https")
            || self.cfg.url.as_deref().is_some_and(|u| u.starts_with("https://"));
        if https {
            "; Secure"
        } else {
            ""
        }
    }
}

fn redirect(to: &str, cookie: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, to.to_owned()),
            (header::SET_COOKIE, cookie.to_owned()),
        ],
    )
        .into_response()
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('=').map(str::to_owned))
}

/// `https://a.exemplo.com:8443/x` -> `a.exemplo.com`.
fn host_of_url(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    host_of_authority(rest.split('/').next().unwrap_or(rest))
}

/// `host:porta` -> `host` (mantém os colchetes de um IPv6 literal).
fn host_of_authority(a: &str) -> &str {
    if a.starts_with('[') {
        a.split_inclusive(']').next().unwrap_or(a)
    } else {
        a.split(':').next().unwrap_or(a)
    }
}

/// IP do usuário. Atrás do proxy local o par é 127.0.0.1; aí vale o último
/// valor de `X-Forwarded-For` (o que o *nosso* proxy acrescentou). De um par
/// que não é local o cabeçalho é ignorado: qualquer um poderia forjá-lo.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if peer.ip().is_loopback() {
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
    }
    peer.ip()
}

/// Proteção contra CSRF além do `SameSite=Strict` do cookie: o `Origin` precisa
/// ser este mesmo site. Sem `Origin` (alguns navegadores omitem em GET/POST do
/// próprio site) aceitamos só o que o navegador declara como mesma origem.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        Some(origin) => origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .is_some_and(|o| o == host),
        None => headers
            .get("sec-fetch-site")
            .is_some_and(|v| v == "same-origin" || v == "none"),
    }
}

/// Ações do painel só aceitam chamadas do htmx: outro site não consegue enviar
/// esse cabeçalho sem um preflight de CORS que o servidor nunca aprova.
pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("hx-request").is_some_and(|v| v == "true")
}

/// Limite de tentativas por IP e tipo (login, cadastro), em memória.
#[derive(Default)]
pub struct Limiter {
    hits: Mutex<HashMap<(&'static str, IpAddr), Vec<Instant>>>,
}

impl Limiter {
    /// Registra a tentativa e diz se ainda cabe em `max` por `window_secs`.
    pub fn allow(&self, kind: &'static str, ip: IpAddr, max: usize, window_secs: u64) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(window_secs);
        let mut hits = self.hits.lock().unwrap();
        if hits.len() > 10_000 {
            hits.retain(|_, v| v.last().is_some_and(|t| now.duration_since(*t) < window));
        }
        let v = hits.entry((kind, ip)).or_default();
        v.retain(|t| now.duration_since(*t) < window);
        if v.len() >= max {
            return false;
        }
        v.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn origin_must_match_the_host() {
        let ok = headers(&[("host", "t.exemplo.com"), ("origin", "https://t.exemplo.com")]);
        assert!(same_origin(&ok));
        let other = headers(&[("host", "t.exemplo.com"), ("origin", "https://evil.com")]);
        assert!(!same_origin(&other));
        let prefix = headers(&[
            ("host", "t.exemplo.com"),
            ("origin", "https://t.exemplo.com.evil.com"),
        ]);
        assert!(!same_origin(&prefix));
        let null = headers(&[("host", "t.exemplo.com"), ("origin", "null")]);
        assert!(!same_origin(&null));
    }

    #[test]
    fn without_origin_only_same_origin_fetch_metadata_passes() {
        assert!(same_origin(&headers(&[
            ("host", "h"),
            ("sec-fetch-site", "same-origin")
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "h"),
            ("sec-fetch-site", "cross-site")
        ])));
        assert!(!same_origin(&headers(&[("host", "h")])));
    }

    #[test]
    fn htmx_header_is_required() {
        assert!(is_htmx(&headers(&[("hx-request", "true")])));
        assert!(!is_htmx(&headers(&[])));
    }

    #[test]
    fn cookie_is_found_among_others() {
        let h = headers(&[("cookie", "a=1; hbt_session=abc; b=2")]);
        assert_eq!(cookie_value(&h, COOKIE).as_deref(), Some("abc"));
        assert_eq!(
            cookie_value(&headers(&[("cookie", "xhbt_session=zzz")]), COOKIE),
            None
        );
    }

    #[test]
    fn hosts_are_extracted_from_urls_and_authorities() {
        assert_eq!(host_of_url("https://meuhbt.duckdns.org"), "meuhbt.duckdns.org");
        assert_eq!(
            host_of_url("https://meuhbt.duckdns.org:8443/x"),
            "meuhbt.duckdns.org"
        );
        assert_eq!(host_of_authority("1.2.3.4:8081"), "1.2.3.4");
        assert_eq!(host_of_authority("[::1]:8081"), "[::1]");
    }

    #[test]
    fn forwarded_for_is_trusted_only_from_a_local_proxy() {
        let h = headers(&[("x-forwarded-for", "9.9.9.9, 1.2.3.4")]);
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let remote: SocketAddr = "8.8.8.8:5000".parse().unwrap();
        assert_eq!(client_ip(local, &h), "1.2.3.4".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip(remote, &h), "8.8.8.8".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn limiter_blocks_after_max_per_window() {
        let l = Limiter::default();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..3 {
            assert!(l.allow("login", ip, 3, 60));
        }
        assert!(!l.allow("login", ip, 3, 60));
        assert!(l.allow("signup", ip, 3, 60), "tipos separados");
        assert!(
            l.allow("login", "5.6.7.8".parse().unwrap(), 3, 60),
            "IPs separados"
        );
    }
}
