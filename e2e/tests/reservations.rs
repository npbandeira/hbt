//! Subdomínios reservados pelo site: só a conta dona abre túnel com o nome.

mod common;

use common::*;

use std::time::Duration;

use tokio::time::timeout;

const PASSWORD: &str = "uma-senha-bem-longa";

async fn account(web: u16, email: &str) -> (Browser, String) {
    let mut b = Browser::new(web);
    assert_eq!(b.signup(email, PASSWORD).await.status, 303);
    let r = b.create_token("notebook").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let (_, token, _) = parse_configure(&r.body);
    (b, token)
}

async fn reserve(b: &mut Browser, name: &str) -> Resp {
    b.send("POST", "/reservations", &form(&[("name", name)]), true)
        .await
}

#[tokio::test]
async fn a_reserved_subdomain_only_opens_for_its_account() {
    let (edge, public) = (free_port(), free_port());
    let edge_bind = format!("127.0.0.1:{edge}");
    let (fx, web) = Fixture::with_web(
        &format!("{public}-{public}"),
        &[
            "--domain",
            "localhost",
            "--http-bind",
            &edge_bind,
            "--max-reserved-per-user",
            "2",
        ],
    )
    .await;
    let (mut ana, ana_token) = account(web, "ana@exemplo.com").await;
    let (mut bia, bia_token) = account(web, "bia@exemplo.com").await;

    let r = reserve(&mut ana, "Ana-App").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.body.contains("http://ana-app.localhost:"),
        "nome normalizado: {}",
        r.body
    );
    assert!(ana
        .send("GET", "/dashboard", "", false)
        .await
        .body
        .contains("Subdomínios reservados"));

    // Outra conta não reserva o mesmo nome, nem nomes do servidor; a cota vale.
    assert!(reserve(&mut bia, "ana-app")
        .await
        .body
        .contains("já é de outra conta"));
    assert!(reserve(&mut bia, "www").await.body.contains("nome do servidor"));
    assert!(reserve(&mut bia, "-x").await.body.contains("letras minúsculas"));
    assert_eq!(reserve(&mut ana, "ana-2").await.status, 200);
    assert!(reserve(&mut ana, "ana-3").await.body.contains("já reservou 2"));

    // Bia não abre túnel com o nome da Ana: o servidor recusa e o cliente
    // segue tentando, sem nunca imprimir o banner.
    let site = http_ok_server().await;
    let mut c = fx.client(&bia_token, &["http", &site.to_string(), "--subdomain", "ana-app"]);
    c.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut intruder = c.spawn().unwrap();
    let mut out = String::new();
    let _ = timeout(Duration::from_secs(2), async {
        use tokio::io::AsyncReadExt;
        let _ = intruder.stdout.as_mut().unwrap().read_to_string(&mut out).await;
    })
    .await;
    assert!(!out.contains("Forwarding"), "túnel de outra conta abriu: {out}");
    drop(intruder);

    // A Ana abre, e o painel mostra o nome em uso.
    let (_client, url) =
        banner_url(fx.client(&ana_token, &["http", &site.to_string(), "--subdomain", "ana-app"])).await;
    assert_eq!(url, format!("http://ana-app.localhost:{edge}"));
    assert!(http_wait(edge, "ana-app.localhost", "200 OK")
        .await
        .ends_with("ok"));
    assert!(ana
        .send("GET", "/dashboard", "", false)
        .await
        .body
        .contains("Em uso"));

    // Liberar: só a dona; depois disso o nome volta a ser de quem chegar.
    assert_eq!(
        bia.send("DELETE", "/reservations/ana-2", "", true).await.status,
        404
    );
    assert_eq!(
        ana.send("DELETE", "/reservations/ana-2", "", true).await.status,
        200
    );
    assert_eq!(reserve(&mut bia, "ana-2").await.status, 200);
}
