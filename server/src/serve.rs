//! Inicialização do servidor: listeners e loop de aceitação da porta de controle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use hbt_proto::quic;
use tokio::net::{lookup_host, TcpListener};
use tracing::{debug, info, warn};

use crate::cli::{ServeArgs, Signup};
use crate::control::handle_conn;
use crate::db::Store;
use crate::http_edge::http_edge;
use crate::identity::Identity;
use crate::metrics;
use crate::state::{HttpCfg, State};
use crate::web::{self, WebCfg};

pub async fn serve(args: ServeArgs, store: Store, identity: Identity) -> Result<()> {
    let http_listener = match args.domain {
        Some(_) => Some(
            TcpListener::bind(&args.http_bind)
                .await
                .with_context(|| format!("não consegui abrir {}", args.http_bind))?,
        ),
        None => None,
    };
    let http = match (&args.domain, &http_listener) {
        (Some(domain), Some(l)) => Some(HttpCfg {
            domain: domain.to_ascii_lowercase(),
            port: l.local_addr()?.port(),
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
        let web = web::new(
            state.clone(),
            WebCfg {
                url: args.web_url.clone(),
                signup_open: args.signup == Signup::Open,
                max_tokens: args.max_tokens_per_user,
                control_port: control_addr.port(),
                fingerprint: identity.fingerprint(),
            },
        );
        tokio::spawn(web::serve(l, web));
    }

    if let Some(l) = http_listener {
        info!(
            "HTTP em {} | *.{}",
            args.http_bind,
            state.http.as_ref().unwrap().domain
        );
        tokio::spawn(http_edge(l, state.clone()));
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
