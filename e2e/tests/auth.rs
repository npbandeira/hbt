//! Autenticação, ACLs e revogação de tokens.

mod common;

use common::*;

use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn wrong_secret_makes_client_exit_with_error() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let (id, _) = token.split_once('.').unwrap();
    let forged = format!("{id}.{}", "0".repeat(64)); // id certo, segredo errado
    let mut client = spawn_quiet(fx.client(&forged, &["tcp", "1"]));
    let st = timeout(Duration::from_secs(10), client.wait())
        .await
        .expect("cliente deveria sair")
        .unwrap();
    assert!(!st.success());
}

#[tokio::test]
async fn unknown_token_id_is_rejected_like_a_wrong_secret() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    fx.token("t", &[]);
    let forged = format!("deadbeef.{}", "1".repeat(64));
    let mut client = spawn_quiet(fx.client(&forged, &["tcp", "1"]));
    let st = timeout(Duration::from_secs(10), client.wait())
        .await
        .expect("cliente deveria sair")
        .unwrap();
    assert!(!st.success());
}

#[tokio::test]
async fn token_port_acl_is_enforced() {
    let base = free_port();
    let (ok_port, bad_port) = (base, base.saturating_add(1));
    // Servidor permite as duas; o TOKEN só permite a primeira.
    let fx = Fixture::start(&format!("{ok_port}-{bad_port}"), &[]);
    let token = fx.token("restrito", &["--ports", &format!("{ok_port}-{ok_port}")]);
    let echo = tcp_echo_server().await;
    let e = echo.to_string();

    let _denied = spawn_quiet(fx.client(&token, &["tcp", &e, "--remote-port", &bad_port.to_string()]));
    sleep(Duration::from_millis(1500)).await;
    assert!(
        TcpStream::connect(("127.0.0.1", bad_port)).await.is_err(),
        "porta fora do token abriu"
    );

    let _allowed = spawn_quiet(fx.client(&token, &["tcp", &e, "--remote-port", &ok_port.to_string()]));
    assert!(
        tcp_echo_works(ok_port).await,
        "porta permitida pelo token não funcionou"
    );
}

#[tokio::test]
async fn token_subdomain_acl_is_enforced() {
    let (fx, edge) = http_fixture();
    let token = fx.token("ana", &["--subdomains", "ana"]);
    let web = http_ok_server().await;
    let w = web.to_string();

    // Pedir subdomínio fora da lista: nunca roteia.
    let mut denied = spawn_quiet(fx.client(&token, &["http", &w, "--subdomain", "outro"]));
    sleep(Duration::from_millis(1500)).await;
    assert!(http_get(edge, "outro.localhost")
        .await
        .starts_with("HTTP/1.1 404"));
    let _ = denied.kill().await;

    // Sem pedir nenhum: o servidor usa o da lista ("ana").
    let _ok = spawn_quiet(fx.client(&token, &["http", &w]));
    let resp = http_wait(edge, "ana.localhost", "200 OK").await;
    assert!(resp.contains("200 OK"), "resposta: {resp:?}");
}

#[tokio::test]
async fn revoking_a_token_drops_the_live_session_and_blocks_reconnect() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("vai-ser-revogado", &[]);
    let echo = tcp_echo_server().await;
    let mut client = spawn_quiet(fx.client(
        &token,
        &["tcp", &echo.to_string(), "--remote-port", &public.to_string()],
    ));
    assert!(
        tcp_echo_works(public).await,
        "túnel deveria funcionar antes da revogação"
    );

    let out = fx.token_cmd(&["revoke", "vai-ser-revogado"]);
    assert!(out.status.success());

    // Em até ~1 heartbeat o servidor derruba a sessão; o cliente reconecta,
    // é recusado na autenticação (erro fatal) e SAI.
    let st = timeout(Duration::from_secs(30), client.wait())
        .await
        .expect("cliente deveria desistir após a revogação")
        .unwrap();
    assert!(!st.success());
    assert!(
        TcpStream::connect(("127.0.0.1", public)).await.is_err(),
        "porta pública deveria ter sido liberada"
    );
}

#[tokio::test]
async fn token_cli_create_list_revoke() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token(
        "notebook",
        &["--ports", "20000-20010", "--subdomains", "ana,ana-dev"],
    );
    let (id, secret) = token.split_once('.').unwrap();

    let list = String::from_utf8(fx.token_cmd(&["list"]).stdout).unwrap();
    assert!(list.contains("notebook") && list.contains(id) && list.contains("20000-20010"));
    assert!(list.contains("ana,ana-dev"));
    assert!(!list.contains(secret), "list não pode mostrar o segredo");
    let on_disk = std::fs::read_to_string(fx.dir.join("tokens.json")).unwrap();
    assert!(!on_disk.contains(secret), "o segredo cru não pode estar no disco");

    assert!(
        !fx.token_cmd(&["create", "notebook"]).status.success(),
        "nome duplicado"
    );
    assert!(fx.token_cmd(&["revoke", "notebook"]).status.success());
    assert!(!fx.token_cmd(&["revoke", "notebook"]).status.success());
    let list = String::from_utf8(fx.token_cmd(&["list"]).stdout).unwrap();
    assert!(list.contains("nenhum token"));
}

#[tokio::test]
async fn wrong_fingerprint_is_refused_with_a_clear_message() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let mut c = cmd("hbt");
    c.args([
        "--server",
        &fx.addr(),
        "--fingerprint",
        &"00".repeat(32),
        "tcp",
        "1",
    ])
    .env("HBT_TOKEN", &token)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let mut client = c.spawn().unwrap();
    // Os avisos (tracing) do cliente saem no stdout.
    let mut out = tokio::io::BufReader::new(client.stdout.take().unwrap());
    let mut line = String::new();
    timeout(
        Duration::from_secs(15),
        tokio::io::AsyncBufReadExt::read_line(&mut out, &mut line),
    )
    .await
    .expect("cliente deveria avisar do handshake")
    .unwrap();
    assert!(
        line.contains("fingerprint"),
        "mensagem sem pista do fingerprint: {line}"
    );
    // Sem túnel registrado: o servidor não deve ter aberto a porta pública.
    assert!(TcpStream::connect(("127.0.0.1", public)).await.is_err());
}
