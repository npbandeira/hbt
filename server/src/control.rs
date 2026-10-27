//! Porta de controle: handshake, autenticação e registro do túnel.

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hbt_proto::mux::{self, Opener};
use hbt_proto::*;
use rand::Rng;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::time::timeout;
use tracing::info;

use crate::state::{RouteGuard, State};
use crate::tokens::TokenRecord;
use crate::tunnel::run_tunnel;

const OLD_CLIENT: &str = "este cliente hbt é antigo (uma conexão por usuário); \
     atualize o hbt para a versão com multiplexação";

/// Uma conexão na porta de controle: handshake e, se der certo, o túnel.
pub async fn handle_conn(mut stream: TcpStream, state: Arc<State>) -> Result<()> {
    let hello: Hello = timeout(Duration::from_secs(5), read_msg(&mut stream))
        .await
        .context("timeout esperando Hello")??;
    match hello {
        // O cliente antigo só mostra (e desiste com) um `Error` recebido
        // DEPOIS do desafio; antes disso ele só tentaria de novo para sempre.
        Hello::V1Control { .. } => {
            state.metrics.handshake_failed();
            write_msg(&mut stream, &ServerMsg::Challenge(rand::random())).await?;
            let _ = timeout(Duration::from_secs(5), read_msg::<_, ClientMsg>(&mut stream)).await;
            let _ = write_msg(&mut stream, &ServerMsg::Error(OLD_CLIENT.into())).await;
            bail!("cliente com protocolo antigo recusado")
        }
        Hello::V1Accept(_) => bail!("conexão de dados do protocolo antigo"),
        Hello::Control { token_id } => {
            // Qualquer erro antes de "Registered" é reportado ao cliente.
            let (opener, requests) = mux::opener();
            match timeout(
                Duration::from_secs(5),
                negotiate(&mut stream, &state, &opener, &token_id),
            )
            .await
            {
                Ok(Ok((bound, rec))) => run_tunnel(stream, state, bound, rec, opener, requests).await,
                Ok(Err(e)) => {
                    state.metrics.handshake_failed();
                    let _ = write_msg(&mut stream, &ServerMsg::Error(e.to_string())).await;
                    Err(e)
                }
                Err(_) => {
                    state.metrics.handshake_failed();
                    bail!("timeout no handshake")
                }
            }
        }
    }
}

/// O recurso público que o túnel expõe.
pub enum Bound {
    Tcp(TcpListener),
    Udp(UdpSocket),
    /// Sem socket próprio: só mantém a rota registrada enquanto viver.
    Http(RouteGuard),
}

/// Autentica o cliente e abre a porta pública pedida.
async fn negotiate(
    stream: &mut TcpStream,
    state: &Arc<State>,
    opener: &Opener,
    token_id: &str,
) -> Result<(Bound, TokenRecord)> {
    // Id desconhecido recebe o desafio igual a um id válido e falha só depois
    // da verificação, com a MESMA mensagem: assim não dá para descobrir quais
    // ids existem (user enumeration) olhando respostas ou tempo.
    let record = state.tokens.find(token_id).await?;
    let key = record
        .as_ref()
        .and_then(|r| hex_decode(&r.key))
        .unwrap_or_else(|| vec![0u8; 32]);

    let nonce: [u8; 32] = rand::random();
    write_msg(stream, &ServerMsg::Challenge(nonce)).await?;

    let ClientMsg::Auth(mac) = read_msg(stream).await? else {
        bail!("esperava Auth");
    };
    let valid = verify_auth(&key, &nonce, &mac);
    let Some(record) = record.filter(|_| valid) else {
        state.metrics.auth_failed();
        bail!("autenticação falhou");
    };
    write_msg(stream, &ServerMsg::AuthOk).await?;
    info!("cliente '{}' autenticado", record.name);

    let ClientMsg::Register(spec) = read_msg(stream).await? else {
        bail!("esperava Register");
    };
    info!("registro de '{}': {spec:?}", record.name);
    let (bound, public) = match spec {
        TunnelSpec::Tcp { remote_port } => {
            let range = port_range_for(state, &record)?;
            let (l, port) = bind_port(state.tunnel_ip, &range, remote_port, TcpListener::bind).await?;
            (Bound::Tcp(l), Public::Port(port))
        }
        TunnelSpec::Udp { remote_port } => {
            let range = port_range_for(state, &record)?;
            let (s, port) = bind_port(state.tunnel_ip, &range, remote_port, UdpSocket::bind).await?;
            (Bound::Udp(s), Public::Port(port))
        }
        TunnelSpec::Http { subdomain } => register_http(state, opener, subdomain, &record)?,
    };
    write_msg(stream, &ServerMsg::Registered { public }).await?;
    Ok((bound, record))
}

/// Faixa que ESTE cliente pode usar: faixa global do servidor ∩ faixa do token.
fn port_range_for(state: &State, record: &TokenRecord) -> Result<RangeInclusive<u16>> {
    record.effective_ports(&state.port_range).with_context(|| {
        format!(
            "o token '{}' não permite nenhuma porta da faixa do servidor",
            record.name
        )
    })
}

/// Abre a porta pedida (validando a faixa) ou, se o cliente não pediu
/// nenhuma, sorteia portas da faixa até achar uma livre.
///
/// Genérico sobre COMO abrir (`bind`): serve para TCP e UDP sem duplicar
/// código. `F` é qualquer função/closure `endereço -> Future<io::Result<T>>`;
/// `TcpListener::bind` e `UdpSocket::bind` se encaixam direto.
async fn bind_port<T, F, Fut>(
    ip: IpAddr,
    range: &RangeInclusive<u16>,
    requested: Option<u16>,
    bind: F,
) -> Result<(T, u16)>
where
    F: Fn((IpAddr, u16)) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    if let Some(port) = requested {
        if !range.contains(&port) {
            bail!("porta {port} fora da faixa permitida para este token ({range:?})");
        }
        let t = bind((ip, port))
            .await
            .with_context(|| format!("porta {port} indisponível"))?;
        return Ok((t, port));
    }
    for _ in 0..100 {
        // `thread_rng()` não é Send: por isso o gerador vive só nesta linha,
        // nunca atravessa um `.await`.
        let port = rand::thread_rng().gen_range(range.clone());
        if let Ok(t) = bind((ip, port)).await {
            return Ok((t, port));
        }
    }
    bail!("nenhuma porta livre na faixa {range:?}")
}

/// Subdomínio vira parte de um nome DNS: só `a-z 0-9 -`, 1 a 63 caracteres,
/// sem hífen nas pontas (regra de "label" DNS).
pub fn validate_subdomain(s: &str) -> Result<()> {
    let ok = (1..=63).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-');
    if !ok {
        bail!("subdomínio inválido (use a-z, 0-9 e '-', até 63 caracteres)");
    }
    Ok(())
}

fn register_http(
    state: &Arc<State>,
    opener: &Opener,
    requested: Option<String>,
    record: &TokenRecord,
) -> Result<(Bound, Public)> {
    let Some(cfg) = &state.http else {
        bail!("túneis HTTP desabilitados neste servidor (falta --domain)");
    };
    if let Some(s) = &requested {
        validate_subdomain(s)?;
    }
    // Checar e inserir sob o MESMO lock: senão dois clientes poderiam
    // checar "livre" ao mesmo tempo e ambos registrar (race condition).
    let mut routes = state.routes.lock().unwrap();
    // Token com lista de subdomínios: só estes. Sem pedir nenhum, usa o
    // primeiro livre da lista (em vez de sortear um que não seria dele).
    let sub = match (requested, &record.subdomains) {
        (Some(s), Some(list)) if !list.contains(&s) => {
            bail!("subdomínio '{s}' não é permitido para o token '{}'", record.name)
        }
        (Some(s), _) => s,
        (None, Some(list)) => list
            .iter()
            .find(|c| !routes.contains_key(*c))
            .cloned()
            .context("todos os subdomínios deste token estão em uso")?,
        (None, None) => loop {
            let candidate = format!("{:08x}", rand::random::<u32>());
            if !routes.contains_key(&candidate) {
                break candidate;
            }
        },
    };
    if routes.contains_key(&sub) {
        bail!("subdomínio '{sub}' já está em uso");
    }
    routes.insert(sub.clone(), opener.clone());
    drop(routes);

    let port = if cfg.port == 80 {
        String::new()
    } else {
        format!(":{}", cfg.port)
    };
    let url = format!("http://{sub}.{}{port}", cfg.domain);
    let guard = RouteGuard {
        state: state.clone(),
        subdomain: sub,
    };
    Ok((Bound::Http(guard), Public::Url(url)))
}
