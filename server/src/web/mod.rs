//! O site do hbt: landing, cadastro, login e painel de tokens.
//!
//! HTML gerado no servidor (maud, que escapa tudo por padrão) e trocado em
//! pedaços pelo htmx. Quem põe HTTPS na frente é o proxy (Caddy/nginx): o site
//! só fala HTTP em `--web-bind` e confia em `X-Forwarded-*` quando o par é local.

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
    /// Porta do QUIC (controle), para o comando de configuração.
    pub control_port: u16,
    pub fingerprint: String,
}

pub async fn serve(listener: TcpListener, web: Arc<Web>) {
    let app = Router::new()
        .route("/", get(landing))
        .route("/signup", get(signup_page).post(signup))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/dashboard", get(dashboard))
        .route("/tokens", post(create_token))
        .route("/tokens/{id}", delete(revoke_token))
        .route("/assets/{file}", get(assets::serve))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(web);
    if let Err(e) = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await {
        tracing::warn!("site encerrou: {e}");
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
    pages::dashboard(&user.email, &tokens, &online).into_response()
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
