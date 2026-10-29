//! O site: cadastro, login, painel de tokens e as defesas.

mod common;

use common::*;

const PASSWORD: &str = "uma-senha-bem-longa";

fn range() -> String {
    let p = free_port();
    format!("{p}-{p}")
}

#[tokio::test]
async fn landing_and_assets_are_served_with_security_headers() {
    let (_fx, web) = Fixture::with_web(&range(), &[]).await;
    let r = http(web, "GET", "/", &[], "").await;
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Criar conta"));
    let csp = r.header("content-security-policy").expect("CSP");
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(r.header("x-frame-options"), Some("DENY"));
    assert_eq!(r.header("cache-control"), Some("no-store"));

    let js = http(web, "GET", "/assets/htmx.min.js", &[], "").await;
    assert_eq!(js.status, 200);
    assert!(js.header("cache-control").unwrap().contains("immutable"));
    assert_eq!(
        http(web, "GET", "/assets/nao-existe.js", &[], "").await.status,
        404
    );
}

#[tokio::test]
async fn signup_creates_a_token_that_opens_a_real_tunnel() {
    let public = free_port();
    let (fx, web) = Fixture::with_web(&format!("{public}-{public}"), &[]).await;
    let echo = tcp_echo_server().await;
    let mut b = Browser::new(web);

    let r = b.signup("ana@exemplo.com", PASSWORD).await;
    assert_eq!(r.status, 303);
    assert_eq!(r.header("location"), Some("/dashboard"));
    let set_cookie = r.header("set-cookie").unwrap();
    assert!(
        set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"),
        "{set_cookie}"
    );

    let dash = b.send("GET", "/dashboard", "", false).await;
    assert_eq!(dash.status, 200);
    assert!(dash.body.contains("Você ainda não tem tokens"));

    let r = b.create_token("notebook").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let (server, token, fingerprint) = parse_configure(&r.body);
    assert!(
        server.ends_with(&format!(":{}", fx.control)),
        "porta do QUIC no comando: {server}"
    );
    assert_eq!(
        fingerprint, fx.fingerprint,
        "o painel mostra o fingerprint real do servidor"
    );

    // O comando do painel funciona de verdade: túnel TCP ponta a ponta.
    let mut c = cmd("hbt");
    c.args(["--server", &fx.addr(), "--fingerprint", &fingerprint])
        .args(["tcp", &echo.to_string(), "--remote-port", &public.to_string()])
        .env("HBT_TOKEN", &token);
    let _client = spawn_quiet(c);
    assert!(
        tcp_echo_works(public).await,
        "túnel com o token criado no site nunca respondeu"
    );

    // O painel passa a mostrar o token como conectado.
    let id = token.split_once('.').unwrap().0;
    let mut online = false;
    for _ in 0..50 {
        let d = b.send("GET", "/dashboard", "", false).await;
        assert!(d.body.contains(id), "o token novo aparece na lista");
        if d.body.contains("Conectado") {
            online = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(online, "o painel nunca mostrou o token conectado");

    // Revogar pelo painel remove o token.
    let del = b.send("DELETE", &format!("/tokens/{id}"), "", true).await;
    assert_eq!(del.status, 200);
    let d = b.send("GET", "/dashboard", "", false).await;
    assert!(!d.body.contains(id), "o token revogado sai da lista");
    let list = String::from_utf8_lossy(&fx.token_cmd(&["list"]).stdout).into_owned();
    assert!(!list.contains(id), "e do banco:\n{list}");
}

#[tokio::test]
async fn wrong_password_and_unknown_account_get_the_same_answer() {
    let (_fx, web) = Fixture::with_web(&range(), &[]).await;
    let mut b = Browser::new(web);
    b.signup("ana@exemplo.com", PASSWORD).await;

    let mut other = Browser::new(web);
    let wrong = other.login("ana@exemplo.com", "senha-errada-aqui").await;
    let ghost = other.login("ninguem@exemplo.com", "senha-errada-aqui").await;
    for r in [&wrong, &ghost] {
        assert_eq!(r.status, 422);
        assert!(r.header("set-cookie").is_none(), "erro de login não abre sessão");
        assert!(r.body.contains("E-mail ou senha incorretos."));
    }
    let ok = other.login("ANA@exemplo.com", PASSWORD).await;
    assert_eq!(ok.status, 303, "e-mail não diferencia maiúsculas");
}

#[tokio::test]
async fn dashboard_requires_a_session() {
    let (_fx, web) = Fixture::with_web(&range(), &[]).await;
    let r = http(web, "GET", "/dashboard", &[], "").await;
    assert_eq!(r.status, 303);
    assert_eq!(r.header("location"), Some("/login"));
    let forged = http(web, "GET", "/dashboard", &[("Cookie", "hbt_session=00")], "").await;
    assert_eq!(forged.status, 303);
}

#[tokio::test]
async fn signup_validates_email_password_and_duplicates() {
    let (_fx, web) = Fixture::with_web(&range(), &[]).await;
    let mut b = Browser::new(web);
    assert!(b
        .signup("sem-arroba", PASSWORD)
        .await
        .body
        .contains("e-mail válido"));
    assert!(b
        .signup("ana@exemplo.com", "curta")
        .await
        .body
        .contains("pelo menos 10"));
    assert_eq!(b.signup("ana@exemplo.com", PASSWORD).await.status, 303);
    let dup = Browser::new(web).signup("ANA@exemplo.com", PASSWORD).await;
    assert_eq!(dup.status, 422);
    assert!(dup.body.contains("Já existe uma conta"));
}

#[tokio::test]
async fn csrf_defenses_refuse_cross_site_and_non_htmx_requests() {
    let (_fx, web) = Fixture::with_web(&range(), &[]).await;
    let mut b = Browser::new(web);
    b.signup("ana@exemplo.com", PASSWORD).await;
    let cookie = b.cookie.clone().unwrap();
    let body = form(&[("name", "x")]);

    // Origin de outro site: recusado mesmo com cookie e HX-Request.
    let evil = http(
        web,
        "POST",
        "/tokens",
        &[
            ("Origin", "https://evil.com"),
            ("HX-Request", "true"),
            ("Cookie", &cookie),
        ],
        &body,
    )
    .await;
    assert_eq!(evil.status, 403);
    // Mesma origem, mas sem o cabeçalho do htmx: recusado.
    let own = format!("http://127.0.0.1:{web}");
    let plain = http(
        web,
        "POST",
        "/tokens",
        &[("Origin", &own), ("Cookie", &cookie)],
        &body,
    )
    .await;
    assert_eq!(plain.status, 403);
    // Login e logout também conferem a origem.
    let login = http(
        web,
        "POST",
        "/login",
        &[("Origin", "https://evil.com")],
        &form(&[("email", "a@b.co"), ("password", PASSWORD)]),
    )
    .await;
    assert_eq!(login.status, 403);
    let logout = http(
        web,
        "POST",
        "/logout",
        &[("Origin", "https://evil.com"), ("Cookie", &cookie)],
        "",
    )
    .await;
    assert_eq!(logout.status, 403);
    // Nada disso criou token.
    let d = b.send("GET", "/dashboard", "", false).await;
    assert!(d.body.contains("Você ainda não tem tokens"));
}

#[tokio::test]
async fn an_account_cannot_revoke_another_accounts_token() {
    let (_fx, web) = Fixture::with_web(&range(), &[]).await;
    let (mut ana, mut bia) = (Browser::new(web), Browser::new(web));
    ana.signup("ana@exemplo.com", PASSWORD).await;
    bia.signup("bia@exemplo.com", PASSWORD).await;
    let (_, token, _) = parse_configure(&ana.create_token("notebook").await.body);
    let id = token.split_once('.').unwrap().0;

    assert_eq!(
        bia.send("DELETE", &format!("/tokens/{id}"), "", true)
            .await
            .status,
        404
    );
    assert!(ana.send("GET", "/dashboard", "", false).await.body.contains(id));
    assert!(
        !bia.send("GET", "/dashboard", "", false).await.body.contains(id),
        "o token de uma conta não aparece para a outra"
    );
    // Mesmo nome em contas diferentes é permitido.
    assert_eq!(bia.create_token("notebook").await.status, 200);
}

#[tokio::test]
async fn token_quota_per_account_is_enforced() {
    let (_fx, web) = Fixture::with_web(&range(), &["--max-tokens-per-user", "2"]).await;
    let mut b = Browser::new(web);
    b.signup("ana@exemplo.com", PASSWORD).await;
    assert_eq!(b.create_token("um").await.status, 200);
    assert_eq!(b.create_token("dois").await.status, 200);
    let third = b.create_token("tres").await;
    assert_eq!(third.status, 422);
    assert!(third.body.contains("Você já tem 2 tokens"), "{}", third.body);
    assert_eq!(b.create_token("um").await.status, 422, "nome repetido");
}

#[tokio::test]
async fn disabling_an_account_blocks_login_session_and_tokens() {
    let (fx, web) = Fixture::with_web(&range(), &[]).await;
    let mut b = Browser::new(web);
    b.signup("ana@exemplo.com", PASSWORD).await;
    let (_, token, _) = parse_configure(&b.create_token("notebook").await.body);
    let id = token.split_once('.').unwrap().0.to_owned();

    let out = fx.admin(&["user", "disable", "ana@exemplo.com"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 tokens revogados"));

    // A sessão aberta cai na hora (a conta é lida do banco a cada requisição).
    let d = b.send("GET", "/dashboard", "", false).await;
    assert_eq!(d.status, 303);
    assert_eq!(d.header("location"), Some("/login"));
    // E não dá para entrar de novo.
    let login = Browser::new(web).login("ana@exemplo.com", PASSWORD).await;
    assert_eq!(login.status, 422);
    assert!(login.body.contains("bloqueada"));
    // O token foi revogado.
    let list = String::from_utf8_lossy(&fx.token_cmd(&["list"]).stdout).into_owned();
    assert!(!list.contains(&id), "{list}");

    fx.admin(&["user", "enable", "ana@exemplo.com"]);
    assert_eq!(
        Browser::new(web).login("ana@exemplo.com", PASSWORD).await.status,
        303
    );
}

#[tokio::test]
async fn user_supplied_text_is_html_escaped() {
    let (_fx, web) = Fixture::with_web(&range(), &[]).await;
    let mut b = Browser::new(web);
    b.signup("ana@exemplo.com", PASSWORD).await;
    let r = b.create_token("<script>alert(1)</script>").await;
    assert_eq!(r.status, 200);
    assert!(!r.body.contains("<script>alert"), "resposta do POST: {}", r.body);
    let d = b.send("GET", "/dashboard", "", false).await;
    assert!(!d.body.contains("<script>alert"));
    assert!(d.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
}

#[tokio::test]
async fn closed_signup_only_accepts_accounts_made_by_the_admin() {
    let (fx, web) = Fixture::with_web(&range(), &["--signup", "closed"]).await;
    let r = Browser::new(web).signup("ana@exemplo.com", PASSWORD).await;
    assert_eq!(r.status, 422);
    assert!(r.body.contains("não aceita novos cadastros"));

    let out = fx.admin(&["user", "add", "bia@exemplo.com"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let password = text
        .split("senha: ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .expect("senha impressa");
    assert_eq!(
        Browser::new(web).login("bia@exemplo.com", password).await.status,
        303
    );
}

#[tokio::test]
async fn login_attempts_are_rate_limited_per_ip() {
    let (_fx, web) = Fixture::with_web(&range(), &[]).await;
    let mut b = Browser::new(web);
    for _ in 0..10 {
        assert!(b
            .login("a@exemplo.com", "senha-errada-aqui")
            .await
            .body
            .contains("incorretos"));
    }
    let blocked = b.login("a@exemplo.com", "senha-errada-aqui").await;
    assert_eq!(blocked.status, 422);
    assert!(blocked.body.contains("Muitas tentativas"));
}

#[tokio::test]
async fn cli_and_site_creating_tokens_at_once_lose_nothing() {
    let (fx, web) = Fixture::with_web(&range(), &["--max-tokens-per-user", "100"]).await;
    let mut b = Browser::new(web);
    b.signup("ana@exemplo.com", PASSWORD).await;
    let cookie = b.cookie.clone().unwrap();

    let dir = fx.dir.clone();
    let cli = tokio::task::spawn_blocking(move || {
        (0..15).all(|i| {
            std::process::Command::new(bin_dir().join("hbt-server"))
                .arg("--data-dir")
                .arg(&dir)
                .args(["token", "create", &format!("cli-{i}"), "--quiet"])
                .output()
                .unwrap()
                .status
                .success()
        })
    });
    let own = format!("http://127.0.0.1:{web}");
    let mut posts = vec![];
    for i in 0..15 {
        let (own, cookie) = (own.clone(), cookie.clone());
        posts.push(tokio::spawn(async move {
            http(
                web,
                "POST",
                "/tokens",
                &[("Origin", &own), ("HX-Request", "true"), ("Cookie", &cookie)],
                &form(&[("name", &format!("web-{i}"))]),
            )
            .await
            .status
        }));
    }
    for p in posts {
        assert_eq!(p.await.unwrap(), 200);
    }
    assert!(cli.await.unwrap(), "algum `token create` do CLI falhou");

    let list = String::from_utf8_lossy(&fx.token_cmd(&["list"]).stdout).into_owned();
    let n = list
        .lines()
        .filter(|l| l.contains("cli-") || l.contains("web-"))
        .count();
    assert_eq!(n, 30, "escritas perdidas:\n{list}");
}
