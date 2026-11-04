//! HTTPS no edge: túneis em https://<sub>.<domínio>, o site no domínio raiz e
//! a porta 80 redirecionando. O certificado é de uma CA de teste posta em
//! `<data-dir>/tls/` (o caminho sem DuckDNS); o Let's Encrypt não entra aqui.

mod common;

use common::*;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// CA de teste + certificado de `hbt.localhost` e `*.hbt.localhost` assinado por ela.
fn test_pki() -> (Vec<u8>, String, String) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);

    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["hbt.localhost".into(), "*.hbt.localhost".into()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    (ca_cert.der().to_vec(), leaf.pem(), key.serialize_pem())
}

fn connector(ca: &[u8]) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(ca.to_vec())).unwrap();
    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(cfg))
}

/// GET por TLS com SNI = `host` (o certificado é conferido de verdade).
async fn https_get(tls: &TlsConnector, port: u16, host: &str, path: &str) -> String {
    let Ok(tcp) = TcpStream::connect(("127.0.0.1", port)).await else {
        return String::new();
    };
    let name = ServerName::try_from(host.to_owned()).unwrap();
    let mut s = match tls.connect(name, tcp).await {
        Ok(s) => s,
        Err(e) => return format!("TLS: {e}"),
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn https_serves_tunnels_and_site_and_http_redirects() {
    let (ca, cert, key) = test_pki();
    let (edge, tls_port, web, public) = (free_port(), free_port(), free_port(), free_port());
    let fx = Fixture::start_prepared(
        &format!("{public}-{public}"),
        &[
            "--domain",
            "hbt.localhost",
            "--http-bind",
            &format!("127.0.0.1:{edge}"),
            "--https-bind",
            &format!("127.0.0.1:{tls_port}"),
            "--web-bind",
            &format!("127.0.0.1:{web}"),
        ],
        |dir: &Path| {
            std::fs::create_dir_all(dir.join("tls")).unwrap();
            std::fs::write(dir.join("tls/cert.pem"), &cert).unwrap();
            std::fs::write(dir.join("tls/key.pem"), &key).unwrap();
        },
    );
    let tls = connector(&ca);
    let token = fx.token("t", &[]);
    let site = http_ok_server().await;

    // O cliente recebe a URL https.
    let (_client, url) =
        banner_url(fx.client(&token, &["http", &site.to_string(), "--subdomain", "demo"])).await;
    assert_eq!(url, format!("https://demo.hbt.localhost:{tls_port}"));

    // Túnel por HTTPS, com o certificado wildcard conferido.
    let mut resp = String::new();
    for _ in 0..50 {
        resp = https_get(&tls, tls_port, "demo.hbt.localhost", "/").await;
        if resp.contains("200 OK") {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    assert!(
        resp.starts_with("HTTP/1.1 200 OK") && resp.ends_with("ok"),
        "{resp:?}"
    );
    assert!(https_get(&tls, tls_port, "outro.hbt.localhost", "/")
        .await
        .starts_with("HTTP/1.1 404"));

    // O site no domínio raiz, com HSTS.
    let landing = https_get(&tls, tls_port, "hbt.localhost", "/").await;
    assert!(landing.starts_with("HTTP/1.1 200"), "{landing:?}");
    assert!(landing.contains("Criar conta"));
    assert!(landing
        .to_ascii_lowercase()
        .contains("strict-transport-security: max-age=31536000"));

    // Porta 80: redireciona (com caminho), para nomes nossos só.
    let r = http_get(edge, "demo.hbt.localhost").await;
    assert!(r.starts_with("HTTP/1.1 308"), "{r:?}");
    assert!(
        r.contains(&format!("Location: https://demo.hbt.localhost:{tls_port}/\r\n")),
        "{r:?}"
    );
    assert!(http_get(edge, "outro.com").await.starts_with("HTTP/1.1 404"));
}

#[tokio::test]
async fn without_a_certificate_tunnels_stay_on_http() {
    let (edge, tls_port, public) = (free_port(), free_port(), free_port());
    let fx = Fixture::start(
        &format!("{public}-{public}"),
        &[
            "--domain",
            "localhost",
            "--http-bind",
            &format!("127.0.0.1:{edge}"),
            "--https-bind",
            &format!("127.0.0.1:{tls_port}"),
        ],
    );
    let token = fx.token("t", &[]);
    let site = http_ok_server().await;
    let (_client, url) =
        banner_url(fx.client(&token, &["http", &site.to_string(), "--subdomain", "demo"])).await;
    assert_eq!(url, format!("http://demo.localhost:{edge}"));
    let resp = http_wait(edge, "demo.localhost", "200 OK").await;
    assert!(
        resp.ends_with("ok"),
        "sem certificado, nada de redirecionar: {resp:?}"
    );
}

#[tokio::test]
async fn duckdns_token_requires_a_duckdns_domain() {
    let public = free_port();
    let mut c = cmd("hbt-server");
    let dir = std::env::temp_dir().join(format!("hbt-e2e-duck-{}-{public}", std::process::id()));
    c.arg("--data-dir").arg(&dir).args([
        "serve",
        "--domain",
        "tunnel.exemplo.com",
        "--http-bind",
        &format!("127.0.0.1:{}", free_port()),
        "--https-bind",
        &format!("127.0.0.1:{}", free_port()),
        "--duckdns-token",
        "x",
    ]);
    let out = c.output().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("duckdns.org"));
}
