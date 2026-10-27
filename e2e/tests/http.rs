//! Túneis HTTP: roteamento por Host.

mod common;

use common::*;

use std::time::Duration;

use tokio::time::timeout;

#[tokio::test]
async fn http_tunnel_routes_by_host_header() {
    let (fx, edge) = http_fixture();
    let token = fx.token("t", &[]);
    let web = http_ok_server().await;
    let _client = spawn_quiet(fx.client(&token, &["http", &web.to_string(), "--subdomain", "demo"]));

    // Com :porta no Host também deve rotear.
    let resp = http_wait(edge, &format!("demo.localhost:{edge}"), "200 OK").await;
    assert!(
        resp.starts_with("HTTP/1.1 200 OK") && resp.ends_with("ok"),
        "resposta: {resp:?}"
    );
    assert!(
        http_get(edge, "DEMO.localhost").await.contains("200 OK"),
        "Host é case-insensitive"
    );

    // Subdomínio sem túnel e domínio errado => 404.
    assert!(http_get(edge, "outro.localhost")
        .await
        .starts_with("HTTP/1.1 404"));
    assert!(http_get(edge, "demo.exemplo.com")
        .await
        .starts_with("HTTP/1.1 404"));
}

#[tokio::test]
async fn http_is_refused_when_server_has_no_domain() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let mut client = spawn_quiet(fx.client(&token, &["http", "1"]));
    // Recusa é não-fatal (o cliente tenta de novo), então ele NÃO deve sair sozinho.
    assert!(timeout(Duration::from_secs(2), client.wait()).await.is_err());
}
