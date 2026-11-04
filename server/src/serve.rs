//! Inicialização do servidor: listeners e loop de aceitação da porta de controle.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use hbt_proto::quic;
use tokio::net::{lookup_host, TcpListener};
use tracing::{debug, info, warn};

use crate::cli::{ServeArgs, Signup};
use crate::control::handle_conn;
use crate::db::Store;
use crate::http_edge::{http_edge, https_edge};
use crate::identity::Identity;
use crate::metrics;
use crate::state::{HttpCfg, HttpsCfg, State};
use crate::tls::{self, Acme, CertStore};
use crate::web::{self, WebCfg};

pub async fn serve(args: ServeArgs, store: Store, identity: Identity, data_dir: &Path) -> Result<()> {
    let domain = args.domain.as_ref().map(|d| d.to_ascii_lowercase());
    if domain.is_none() && (args.https_bind.is_some() || args.duckdns_token.is_some()) {
        bail!("--https-bind e --duckdns-token precisam de --domain");
    }
    if args.duckdns_token.is_some() && args.https_bind.is_none() {
        bail!("--duckdns-token emite o certificado do HTTPS: use junto com --https-bind 0.0.0.0:443");
    }
    if let (Some(d), Some(_)) = (&domain, &args.duckdns_token) {
        tls::duckdns_name(d)?;
    }

    let http_listener = match domain {
        Some(_) => Some(bind(&args.http_bind).await?),
        None => None,
    };
    let https_listener = match &args.https_bind {
        Some(b) => Some(bind(b).await?),
        None => None,
    };
    let https = match &https_listener {
        Some(l) => Some(HttpsCfg {
            port: l.local_addr()?.port(),
            certs: CertStore::open(data_dir)?,
        }),
        None => None,
    };
    let http = match (&domain, &http_listener) {
        (Some(domain), Some(l)) => Some(HttpCfg {
            domain: domain.clone(),
            port: l.local_addr()?.port(),
            https,
        }),
        _ => None,
    };

    let state = Arc::new(State {
        store,
        tunnel_ip: args.tunnel_ip,
        port_range: args.port_range,
        http,
        routes: Mutex::new(HashMap::new()),
        metrics: Arc::default(),
        online: Mutex::default(),
        max_tunnels: args.max_tunnels_per_token,
    });

    let control_addr = lookup_host(&args.control_bind)
        .await
        .and_then(|mut a| a.next().ok_or_else(|| std::io::Error::other("sem endereço")))
        .with_context(|| format!("endereço de controle inválido: {}", args.control_bind))?;
    let endpoint = quinn::Endpoint::server(
        quic::server_config(identity.cert.clone(), identity.key.clone())?,
        control_addr,
    )
    .with_context(|| format!("não consegui abrir {} (UDP)", args.control_bind))?;
    info!(
        "controle em {} (QUIC/UDP) | fingerprint {} | portas públicas {:?}",
        args.control_bind,
        identity.fingerprint(),
        state.port_range
    );
    match state.store.find("").await {
        Err(e) => bail!("não consigo ler os tokens: {e:#}"),
        Ok(_) if args.web_bind.is_none() && state.store.load()?.is_empty() => warn!(
            "nenhum token cadastrado: ninguém consegue conectar. Crie com: hbt-server token create <nome>"
        ),
        Ok(_) => {}
    }

    if let Some(bind) = &args.metrics_bind {
        let l = TcpListener::bind(bind)
            .await
            .with_context(|| format!("não consegui abrir {bind}"))?;
        info!("métricas em http://{bind}/metrics");
        tokio::spawn(metrics::serve(l, state.metrics.clone()));
    }

    tokio::spawn(expire_reservations(state.clone(), args.reservation_days));

    let mut site = None;
    if let Some(bind) = &args.web_bind {
        let l = TcpListener::bind(bind)
            .await
            .with_context(|| format!("não consegui abrir {bind}"))?;
        info!(
            "site em http://{bind} (cadastro {}; até {} tokens por conta)",
            if args.signup == Signup::Open {
                "aberto"
            } else {
                "fechado"
            },
            args.max_tokens_per_user
        );
        // Com HTTPS próprio, o site mora em https://<domínio>.
        let url = args.web_url.clone().or_else(|| {
            let h = state.http.as_ref()?;
            let port = h.https.as_ref()?.port;
            Some(if port == 443 {
                format!("https://{}", h.domain)
            } else {
                format!("https://{}:{port}", h.domain)
            })
        });
        let web = web::new(
            state.clone(),
            WebCfg {
                url,
                signup_open: args.signup == Signup::Open,
                max_tokens: args.max_tokens_per_user,
                max_reserved: args.max_reserved_per_user,
                reservation_days: args.reservation_days,
                control_port: control_addr.port(),
                fingerprint: identity.fingerprint(),
            },
        );
        let app = web::router(web);
        site = Some(app.clone());
        tokio::spawn(web::serve(l, app));
    }

    if let Some(l) = http_listener {
        info!(
            "HTTP em {} | *.{}",
            args.http_bind,
            state.http.as_ref().unwrap().domain
        );
        tokio::spawn(http_edge(l, state.clone()));
    }

    if let (Some(l), Some(h)) = (https_listener, state.http.as_ref()) {
        let certs = h.https.as_ref().unwrap().certs.clone();
        info!(
            "HTTPS em {} | {} e *.{} | certificado {}",
            args.https_bind.as_deref().unwrap_or_default(),
            h.domain,
            h.domain,
            match (&args.duckdns_token, certs.ready()) {
                (_, true) => "carregado",
                (Some(_), false) => "sendo emitido (DuckDNS + Let's Encrypt)",
                (None, false) => "ausente: ponha cert.pem e key.pem em <data-dir>/tls/",
            }
        );
        let acme = args.duckdns_token.clone().map(|token| Acme {
            domain: h.domain.clone(),
            duckdns_token: token,
            email: args.acme_email.clone(),
            staging: args.acme_staging,
        });
        tokio::spawn(tls::keep_fresh(certs, acme));
        tokio::spawn(https_edge(l, state.clone(), site));
    }

    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone(); // clona o Arc (barato), não o State
        let addr = incoming.remote_address();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(incoming, state).await {
                debug!("{addr}: {e:#}");
            }
        });
    }
    Ok(())
}

async fn bind(addr: &str) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("não consegui abrir {addr}"))
}

/// De hora em hora, libera as reservas sem túnel aberto há `days` dias.
async fn expire_reservations(state: Arc<State>, days: u64) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
    loop {
        tick.tick().await;
        let active: Vec<String> = state.routes.lock().unwrap().keys().cloned().collect();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            .saturating_sub(days * 24 * 60 * 60);
        match state
            .store
            .run(move |s| s.expire_reservations(before, &active))
            .await
        {
            Ok(gone) if !gone.is_empty() => info!("reservas liberadas por falta de uso: {}", gone.join(", ")),
            Ok(_) => {}
            Err(e) => warn!("não consegui expirar reservas: {e:#}"),
        }
    }
}
