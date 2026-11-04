//! O site do hbt: landing, cadastro, login e painel de tokens.
//!
//! HTML gerado no servidor (maud, que escapa tudo por padrão) e trocado em
//! pedaços pelo htmx. Em `--web-bind` o site fala HTTP e confia em
//! `X-Forwarded-*` quando o par é local (proxy na frente); com `--https-bind`
//! ele também responde no domínio raiz da porta 443, com o TLS do próprio hbt.

mod assets;
mod auth;
mod pages;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Form, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use maud::Markup;
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::accounts;
use crate::db::StoreError;
use crate::state::State as ServerState;

pub use auth::Web;

/// Configuração do site (vem dos flags do `serve`).
pub struct WebCfg {
    /// URL pública (`https://meuhbt.duckdns.org`); dá o host do comando de
    /// configuração. Sem ela usamos o cabeçalho `Host` da requisição.
    pub url: Option<String>,
    pub signup_open: bool,
    pub max_tokens: usize,
    /// Subdomínios reservados por conta (0 = reservas desligadas).
    pub max_reserved: usize,
    /// Dias sem uso até a reserva ser liberada.
    pub reservation_days: u64,
    /// Porta do QUIC (controle), para o comando de configuração.
    pub control_port: u16,
    pub fingerprint: String,
}

/// As rotas do site, prontas para servir por `serve` (HTTP em `--web-bind`) ou
/// por `serve_conn` (cada conexão TLS do domínio raiz na porta 443).
pub fn router(web: Arc<Web>) -> Router {
    Router::new()
        .route("/", get(landing))
        .route("/signup", get(signup_page).post(signup))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/dashboard", get(dashboard))
        .route("/tokens", post(create_token))
        .route("/tokens/{id}", delete(revoke_token))
        .route("/reservations", post(reserve))
        .route("/reservations/{name}", delete(release))
        .route("/assets/{file}", get(assets::serve))
        .route("/favicon.ico", get(assets::favicon))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(web)
}

pub async fn serve(listener: TcpListener, app: Router) {
    if let Err(e) = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await {
        tracing::warn!("site encerrou: {e}");
    }
}

/// Serve o site numa conexão já com TLS. O IP de quem conectou vai como
/// `ConnectInfo` (o par é o usuário de verdade, sem proxy no meio) e toda
/// resposta leva HSTS: o navegador passa a usar só https neste domínio.
pub async fn serve_conn<IO>(io: IO, peer: SocketAddr, app: Router)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let app = app
        .layer(axum::Extension(ConnectInfo(peer)))
        .layer(middleware::map_response(|mut r: Response| async move {
            r.headers_mut().insert(
                header::STRICT_TRANSPORT_SECURITY,
                HeaderValue::from_static("max-age=31536000"),
            );
            r
        }));
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(std::time::Duration::from_secs(10))
        .serve_connection(
            hyper_util::rt::TokioIo::new(io),
            hyper_util::service::TowerToHyperService::new(app),
        );
    if let Err(e) = conn.await {
        tracing::debug!("site {peer}: {e}");
    }
}

/// Cabeçalhos de defesa em toda resposta. Sem script nem estilo inline: o CSP
/// só aceita arquivos do próprio site (por isso o htmx e o `app.js` são assets).
async fn security_headers(req: axum::extract::Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(
        h,
        "content-security-policy",
        "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    );
    set(h, "x-frame-options", "DENY");
    set(h, "x-content-type-options", "nosniff");
    set(h, "referrer-policy", "no-referrer");
    // Páginas com dados da conta não podem ficar em cache; os assets já definem o deles.
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    res
}

// ---------------------------------------------------------------- páginas

async fn landing(State(web): State<Arc<Web>>) -> Markup {
    pages::landing(web.cfg.signup_open)
}

async fn login_page(State(web): State<Arc<Web>>, headers: HeaderMap) -> Response {
    if web.current_user(&headers).await.is_some() {
        return Redirect::to("/dashboard").into_response();
    }
    pages::login(None, "").into_response()
}

async fn signup_page(State(web): State<Arc<Web>>, headers: HeaderMap) -> Response {
    if web.current_user(&headers).await.is_some() {
        return Redirect::to("/dashboard").into_response();
    }
    pages::signup(web.cfg.signup_open, None, "").into_response()
}

#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
}

/// Erros de formulário voltam como 422: o htmx só troca o conteúdo nesse caso
/// (configurado no `htmx-config`), e sem JavaScript o navegador mostra igual.
fn form_error(page: Markup) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, page).into_response()
}

async fn login(
    State(web): State<Arc<Web>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<Credentials>,
) -> Response {
    if !auth::same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let email = form.email.trim().to_lowercase();
    if !web
        .limiter
        .allow("login", auth::client_ip(peer, &headers), 10, 15 * 60)
    {
        return form_error(pages::login(Some(pages::TOO_MANY), &email));
    }
    if form.password.len() > accounts::MAX_PASSWORD {
        return form_error(pages::login(Some(pages::BAD_LOGIN), &email));
    }
    let lookup = email.clone();
    let user = web
        .state
        .store
        .run(move |s| s.user_by_email(&lookup))
        .await
        .ok()
        .flatten();
    // Conta inexistente verifica contra um hash falso: mesma demora, mesma resposta.
    let hash = user
        .as_ref()
        .map_or_else(|| accounts::dummy_hash().to_owned(), |u| u.password_hash.clone());
    let ok = web.verify(hash, form.password).await;
    match user {
        Some(u) if ok && u.disabled => form_error(pages::login(Some(pages::BLOCKED), &email)),
        Some(u) if ok => web.start_session(&u, &headers),
        _ => form_error(pages::login(Some(pages::BAD_LOGIN), &email)),
    }
}

async fn signup(
    State(web): State<Arc<Web>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<Credentials>,
) -> Response {
    if !auth::same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let open = web.cfg.signup_open;
    let email = form.email.trim().to_lowercase();
    let again = |msg| form_error(pages::signup(open, Some(msg), &email));
    if !open {
        return again(pages::SIGNUP_CLOSED);
    }
    if !web
        .limiter
        .allow("signup", auth::client_ip(peer, &headers), 5, 60 * 60)
    {
        return again(pages::TOO_MANY);
    }
    if !accounts::valid_email(&email) {
        return again(pages::BAD_EMAIL);
    }
    let len = form.password.chars().count();
    if len < accounts::MIN_PASSWORD {
        return again(pages::SHORT_PASSWORD);
    }
    if form.password.len() > accounts::MAX_PASSWORD {
        return again(pages::LONG_PASSWORD);
    }
    let Ok(hash) = web.hash(form.password).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let new_email = email.clone();
    match web.state.store.run(move |s| s.add_user(&new_email, &hash)).await {
        Ok(user) => web.start_session(&user, &headers),
        Err(e) if matches!(e.downcast_ref::<StoreError>(), Some(StoreError::DuplicateEmail)) => {
            again(pages::EMAIL_TAKEN)
        }
        Err(e) => {
            tracing::warn!("cadastro falhou: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn logout(State(web): State<Arc<Web>>, headers: HeaderMap) -> Response {
    if !auth::same_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    web.end_session(&headers)
}

async fn dashboard(State(web): State<Arc<Web>>, headers: HeaderMap) -> Response {
    let Some(user) = web.current_user(&headers).await else {
        return Redirect::to("/login").into_response();
    };
    let owner = user.id.clone();
    let tokens = web
        .state
        .store
        .run(move |s| s.tokens_of(&owner))
        .await
        .unwrap_or_default();
    let online: Vec<bool> = tokens.iter().map(|t| web.state.is_online(&t.id)).collect();
    let reservations = if web.reservations_on() {
        let owner = user.id.clone();
        let list = web
            .state
            .store
            .run(move |s| s.reservations_of(&owner))
            .await
            .unwrap_or_default();
        Some(pages::Reservations {
            rows: list.iter().map(|r| web.reservation_view(r)).collect(),
            max: web.cfg.max_reserved,
            days: web.cfg.reservation_days,
        })
    } else {
        None
    };
    pages::dashboard(&user.email, &tokens, &online, reservations.as_ref()).into_response()
}

#[derive(Deserialize)]
struct NewReservation {
    name: String,
}

async fn reserve(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Form(form): Form<NewReservation>,
) -> Response {
    if !auth::same_origin(&headers) || !auth::is_htmx(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !web.reservations_on() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(user) = web.current_user(&headers).await else {
        return to_login();
    };
    let name = form.name.trim().to_ascii_lowercase();
    if crate::control::validate_subdomain(&name).is_err() {
        return form_error(pages::token_error(pages::BAD_SUBDOMAIN));
    }
    if crate::control::is_banned(&name) {
        return form_error(pages::token_error(&pages::banned(&name)));
    }
    // Em uso agora pelo túnel de outra conta (ou de um token do admin).
    let busy = web
        .state
        .routes
        .lock()
        .unwrap()
        .get(&name)
        .is_some_and(|r| r.owner.as_deref() != Some(user.id.as_str()));
    if busy {
        return form_error(pages::token_error(&pages::in_use(&name)));
    }
    let (owner, max, n) = (user.id.clone(), web.cfg.max_reserved, name.clone());
    match web.state.store.run(move |s| s.reserve(&owner, &n, max)).await {
        Ok(r) => pages::reservation_created(&web.reservation_view(&r)).into_response(),
        Err(e) => {
            let msg = match e.downcast_ref::<StoreError>() {
                Some(StoreError::ReservationQuota(n)) => pages::reservation_quota(*n),
                Some(StoreError::Reserved(_)) => pages::taken(&name),
                _ => {
                    tracing::warn!("reserva falhou: {e:#}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            form_error(pages::token_error(&msg))
        }
    }
}

async fn release(State(web): State<Arc<Web>>, headers: HeaderMap, Path(name): Path<String>) -> Response {
    if !auth::same_origin(&headers) || !auth::is_htmx(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(user) = web.current_user(&headers).await else {
        return to_login();
    };
    match web.state.store.run(move |s| s.release(&user.id, &name)).await {
        // 200 vazio: o htmx troca a linha por nada. Um túnel aberto segue no ar.
        Ok(true) => StatusCode::OK.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

/// Resposta de uma ação htmx sem sessão: manda o navegador para o login.
fn to_login() -> Response {
    (StatusCode::OK, [("hx-redirect", "/login")]).into_response()
}

async fn create_token(
    State(web): State<Arc<Web>>,
    headers: HeaderMap,
    Form(form): Form<NewToken>,
) -> Response {
    if !auth::same_origin(&headers) || !auth::is_htmx(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(user) = web.current_user(&headers).await else {
        return to_login();
    };
    let name = form.name.trim().to_owned();
    let (owner, max, n) = (user.id.clone(), web.cfg.max_tokens, name.clone());
    match web
        .state
        .store
        .run(move |s| s.create(&n, None, None, Some(&owner), Some(max)))
        .await
    {
        Ok(token) => {
            let id = token.split_once('.').map_or("", |(id, _)| id).to_owned();
            let rec = web.state.store.run(move |s| s.get(&id)).await.ok().flatten();
            let Some(rec) = rec else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            let command =
                crate::cli::configure_command(&web.server_addr(&headers), &token, &web.cfg.fingerprint);
            pages::token_created(&rec, &command).into_response()
        }
        Err(e) => {
            let msg = match e.downcast_ref::<StoreError>() {
                Some(StoreError::Quota(n)) => pages::quota(*n),
                Some(StoreError::DuplicateName(_)) => pages::NAME_TAKEN.to_owned(),
                _ if name.is_empty() => pages::NAME_REQUIRED.to_owned(),
                _ => pages::BAD_NAME.to_owned(),
            };
            form_error(pages::token_error(&msg))
        }
    }
}

async fn revoke_token(State(web): State<Arc<Web>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !auth::same_origin(&headers) || !auth::is_htmx(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(user) = web.current_user(&headers).await else {
        return to_login();
    };
    match web.state.store.run(move |s| s.revoke_owned(&user.id, &id)).await {
        // 200 vazio: o htmx troca a linha por nada. Sessões de túnel caem em ~10 s.
        Ok(true) => StatusCode::OK.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Constrói o site a partir do estado do servidor.
pub fn new(state: Arc<ServerState>, cfg: WebCfg) -> Arc<Web> {
    Arc::new(Web::new(state, cfg))
}
