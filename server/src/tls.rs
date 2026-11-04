//! HTTPS do edge público: o certificado que cobre `<domínio>` (o site) e
//! `*.<domínio>` (os túneis), em `<data-dir>/tls/{cert,key}.pem`.
//!
//! Com `--duckdns-token`, o servidor emite e renova o certificado sozinho no
//! Let's Encrypt (ACME, desafio DNS-01 pelo registro TXT do DuckDNS). Sem ele,
//! usa os arquivos que o admin puser lá (gerados por fora, p.ex. com certbot) e
//! os relê de hora em hora.
//!
//! O DuckDNS guarda um único registro TXT por domínio, e o domínio e o wildcard
//! pedem dois valores no mesmo nome (`_acme-challenge.<domínio>`). Por isso as
//! duas validações acontecem uma depois da outra, nunca juntas.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::rustls::{crypto, ServerConfig};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

/// Quanto esperar depois de gravar o TXT antes de pedir a validação: o Let's
/// Encrypt consulta os servidores do DuckDNS de vários pontos da internet.
const DNS_WAIT: Duration = Duration::from_secs(60);
/// Depois de uma falha (DuckDNS fora, limite do Let's Encrypt), tenta de novo em:
const RETRY: Duration = Duration::from_secs(60 * 60);

/// O certificado em uso, trocado sem reiniciar quando é renovado.
pub struct CertStore {
    dir: PathBuf,
    current: RwLock<Option<Loaded>>,
}

#[derive(Clone)]
struct Loaded {
    key: Arc<CertifiedKey>,
    not_before: u64,
    not_after: u64,
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.current.read().unwrap().as_ref().map(|l| l.key.clone())
    }
}

impl CertStore {
    /// Abre `<data-dir>/tls` e carrega o certificado, se já existir.
    pub fn open(data_dir: &Path) -> Result<Arc<CertStore>> {
        let store = Arc::new(CertStore {
            dir: data_dir.join("tls"),
            current: RwLock::new(None),
        });
        if let Err(e) = store.reload() {
            warn!("certificado HTTPS em {} não carregou: {e:#}", store.dir.display());
        }
        Ok(store)
    }

    /// Já há certificado para servir HTTPS?
    pub fn ready(&self) -> bool {
        self.current.read().unwrap().is_some()
    }

    pub fn acceptor(self: &Arc<Self>) -> TlsAcceptor {
        let mut cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(self.clone());
        // Os túneis são canos de HTTP/1.x até o serviço local: nada de h2.
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        TlsAcceptor::from(Arc::new(cfg))
    }

    /// Lê `cert.pem` e `key.pem`. `Ok(false)` se ainda não existem.
    pub fn reload(&self) -> Result<bool> {
        let (cert, key) = (self.dir.join("cert.pem"), self.dir.join("key.pem"));
        let (Ok(cert), Ok(key)) = (fs::read(&cert), fs::read(&key)) else {
            return Ok(false);
        };
        let loaded = parse(&cert, &key)?;
        let changed = self
            .current
            .read()
            .unwrap()
            .as_ref()
            .is_none_or(|c| c.not_after != loaded.not_after);
        if changed {
            info!(
                "certificado HTTPS carregado (vence em {})",
                crate::tokens::format_date(loaded.not_after)
            );
        }
        *self.current.write().unwrap() = Some(loaded);
        Ok(true)
    }

    /// Precisa renovar? Sim sem certificado ou no último terço da validade
    /// (30 dias de 90 no Let's Encrypt; acompanha se a validade encurtar).
    fn due(&self, now: u64) -> bool {
        match &*self.current.read().unwrap() {
            None => true,
            Some(c) => now + (c.not_after.saturating_sub(c.not_before)) / 3 >= c.not_after,
        }
    }

    /// Grava o certificado novo (a chave nasce 0600) e passa a servi-lo.
    fn install(&self, cert_pem: &str, key_pem: &str) -> Result<()> {
        parse(cert_pem.as_bytes(), key_pem.as_bytes())?;
        fs::create_dir_all(&self.dir).with_context(|| format!("criando {}", self.dir.display()))?;
        write_private(&self.dir.join("key.pem"), key_pem.as_bytes())?;
        fs::write(self.dir.join("cert.pem"), cert_pem).context("gravando cert.pem")?;
        self.reload()?;
        Ok(())
    }
}

fn parse(cert_pem: &[u8], key_pem: &[u8]) -> Result<Loaded> {
    let chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .context("cert.pem inválido")?;
    ensure!(!chain.is_empty(), "cert.pem sem certificado");
    let key = PrivateKeyDer::from_pem_slice(key_pem).context("key.pem inválido")?;
    let signer = crypto::ring::sign::any_supported_type(&key).context("tipo de chave não suportado")?;
    let (_, x509) = x509_parser::parse_x509_certificate(&chain[0]).context("cert.pem ilegível")?;
    let validity = x509.validity();
    Ok(Loaded {
        not_before: u64::try_from(validity.not_before.timestamp()).unwrap_or(0),
        not_after: u64::try_from(validity.not_after.timestamp()).unwrap_or(0),
        key: Arc::new(CertifiedKey::new(chain, signer)),
    })
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&tmp)
        .and_then(|mut f| f.write_all(data))
        .with_context(|| format!("gravando {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("gravando {}", path.display()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------- renovação

pub struct Acme {
    pub domain: String,
    pub duckdns_token: String,
    pub email: Option<String>,
    pub staging: bool,
}

/// `meuhbt.duckdns.org` -> `meuhbt` (o nome que a API do DuckDNS espera).
pub fn duckdns_name(domain: &str) -> Result<&str> {
    let name = domain
        .strip_suffix(".duckdns.org")
        .filter(|n| !n.is_empty() && !n.contains('.'))
        .with_context(|| {
            format!(
                "--duckdns-token só serve para um domínio DuckDNS como meuhbt.duckdns.org (recebi {domain})"
            )
        })?;
    Ok(name)
}

/// Mantém o certificado em dia: com ACME emite e renova; sem, só relê os arquivos.
pub async fn keep_fresh(store: Arc<CertStore>, acme: Option<Acme>) {
    loop {
        let wait = match &acme {
            Some(acme) if store.due(now()) => match issue(&store, acme).await {
                Ok(()) => Duration::from_secs(12 * 60 * 60),
                Err(e) => {
                    warn!("não consegui emitir o certificado HTTPS: {e:#} (tento de novo em 1 h)");
                    RETRY
                }
            },
            Some(_) => Duration::from_secs(12 * 60 * 60),
            None => {
                if let Err(e) = store.reload() {
                    warn!("certificado HTTPS em {} não carregou: {e:#}", store.dir.display());
                }
                Duration::from_secs(60 * 60)
            }
        };
        tokio::time::sleep(wait).await;
    }
}

async fn issue(store: &CertStore, acme: &Acme) -> Result<()> {
    let name = duckdns_name(&acme.domain)?;
    info!(
        "pedindo certificado para {0} e *.{0} ao Let's Encrypt",
        acme.domain
    );
    let account = account(store, acme).await?;
    let ids = [
        Identifier::Dns(acme.domain.clone()),
        Identifier::Dns(format!("*.{}", acme.domain)),
    ];
    let mut order = account.new_order(&NewOrder::new(&ids)).await?;
    let result = validate(&mut order, name, &acme.duckdns_token).await;
    // O TXT só serve durante a validação; deixá-lo lá não faz mal, mas limpa.
    let _ = duckdns_txt(name, &acme.duckdns_token, None).await;
    result?;

    let status = order
        .poll_ready(&RetryPolicy::new().timeout(Duration::from_secs(120)))
        .await?;
    ensure!(status == OrderStatus::Ready, "pedido terminou em {status:?}");
    let key_pem = order.finalize().await?;
    let cert_pem = order
        .poll_certificate(&RetryPolicy::new().timeout(Duration::from_secs(120)))
        .await?;
    store.install(&cert_pem, &key_pem)?;
    info!("certificado HTTPS emitido para {0} e *.{0}", acme.domain);
    Ok(())
}

/// Valida as autorizações uma de cada vez (o DuckDNS só tem um TXT).
async fn validate(order: &mut instant_acme::Order, name: &str, token: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15 * 60);
    let mut started: Option<String> = None;
    loop {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "validação DNS demorou demais"
        );
        order.refresh().await?; // relê o pedido e as autorizações
        let mut auths = order.authorizations();
        let mut next = None;
        while let Some(auth) = auths.next().await {
            let mut auth = auth?;
            match auth.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                other => bail!("validação de {} falhou ({other:?})", auth.identifier()),
            }
            if started.as_deref() == Some(auth.url()) {
                next = Some(None); // a que já pedimos ainda está em andamento
                break;
            }
            let url = auth.url().to_owned();
            let mut challenge = auth
                .challenge(ChallengeType::Dns01)
                .context("o Let's Encrypt não ofereceu o desafio DNS-01")?;
            duckdns_txt(name, token, Some(&challenge.key_authorization().dns_value())).await?;
            tokio::time::sleep(DNS_WAIT).await;
            challenge.set_ready().await?;
            next = Some(Some(url));
            break;
        }
        match next {
            None => return Ok(()), // todas válidas
            Some(Some(url)) => started = Some(url),
            Some(None) => {}
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn account(store: &CertStore, acme: &Acme) -> Result<Account> {
    let (file, directory) = if acme.staging {
        ("acme-account-staging.json", LetsEncrypt::Staging.url())
    } else {
        ("acme-account.json", LetsEncrypt::Production.url())
    };
    let path = store.dir.join(file);
    if let Ok(json) = fs::read(&path) {
        let creds: AccountCredentials = serde_json::from_slice(&json).context("conta ACME ilegível")?;
        return Ok(Account::builder()?.from_credentials(creds).await?);
    }
    let contact = acme.email.as_ref().map(|e| format!("mailto:{e}"));
    let contact: Vec<&str> = contact.iter().map(String::as_str).collect();
    let (account, creds) = Account::builder()?
        .create(
            &NewAccount {
                contact: &contact,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            directory.to_owned(),
            None,
        )
        .await?;
    fs::create_dir_all(&store.dir).with_context(|| format!("criando {}", store.dir.display()))?;
    write_private(&path, &serde_json::to_vec(&creds)?)?;
    Ok(account)
}

/// Grava (ou apaga, com `None`) o TXT de `_acme-challenge.<nome>.duckdns.org`.
async fn duckdns_txt(name: &str, token: &str, txt: Option<&str>) -> Result<()> {
    let query = match txt {
        Some(v) => format!("domains={name}&token={token}&txt={v}"),
        None => format!("domains={name}&token={token}&txt=&clear=true"),
    };
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .context("sem certificados raiz do sistema")?
        .https_only()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);
    // A URL leva o token: nunca vai para log nem para a mensagem de erro.
    let uri = format!("https://www.duckdns.org/update?{query}")
        .parse()
        .map_err(|_| anyhow::anyhow!("nome DuckDNS inválido"))?;
    let resp = tokio::time::timeout(Duration::from_secs(30), client.get(uri))
        .await
        .context("DuckDNS não respondeu")?
        .map_err(|_| anyhow::anyhow!("falha ao falar com o DuckDNS"))?;
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|_| anyhow::anyhow!("resposta do DuckDNS cortada"))?;
    let body = String::from_utf8_lossy(&body.to_bytes()).trim().to_owned();
    ensure!(
        body.starts_with("OK"),
        "DuckDNS recusou ({body}): confira o token e o domínio"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdns_name_needs_a_single_duckdns_label() {
        assert_eq!(duckdns_name("meuhbt.duckdns.org").unwrap(), "meuhbt");
        assert!(duckdns_name("a.meuhbt.duckdns.org").is_err());
        assert!(duckdns_name("duckdns.org").is_err());
        assert!(duckdns_name("tunnel.exemplo.com").is_err());
    }

    #[test]
    fn installs_and_renews_in_the_last_third() {
        let dir = std::env::temp_dir().join(format!("hbt-tls-{}", std::process::id()));
        let store = CertStore::open(&dir).unwrap();
        assert!(!store.ready() && store.due(now()));

        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["x.test".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        store.install(&cert.pem(), &key.serialize_pem()).unwrap();
        assert!(store.ready());
        let c = store.current.read().unwrap().clone().unwrap();
        assert!(!store.due(c.not_before));
        assert!(store.due(c.not_after - (c.not_after - c.not_before) / 4));
        let _ = fs::remove_dir_all(&dir);
    }
}
