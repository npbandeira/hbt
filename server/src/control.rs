//! Porta de controle: handshake, autenticação e registro do túnel.

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hbt_proto::mux::{MuxStream, Opener};
use hbt_proto::*;
use rand::Rng;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::time::timeout;
use tracing::info;

use crate::state::{RouteGuard, State};
use crate::tokens::TokenRecord;
use crate::tunnel::run_tunnel;

/// Entrega `msg` ao cliente antes de largar a conexão: largar a `Connection`
/// a fecha na hora e a mensagem se perderia (no TCP o close entregava).
async fn reject(stream: &mut MuxStream, conn: &quinn::Connection, msg: String) {
    let _ = write_msg(stream, &ServerMsg::Error(msg)).await;
    let _ = stream.shutdown().await;
    // O cliente fecha a conexão assim que lê o erro.
    let _ = timeout(Duration::from_secs(2), conn.closed()).await;
}

/// Uma conexão na porta de controle: handshake e, se der certo, o túnel.
pub async fn handle_conn(incoming: quinn::Incoming, state: Arc<State>) -> Result<()> {
    let conn = timeout(Duration::from_secs(5), incoming)
        .await
        .context("timeout no handshake QUIC")??;
    // O primeiro stream do cliente faz o handshake e depois vira o de controle.
    let (send, recv) = timeout(Duration::from_secs(5), conn.accept_bi())
        .await
        .context("timeout esperando o stream de controle")??;
    let mut stream = MuxStream::new(send, recv);
    let hello: Hello = timeout(Duration::from_secs(5), read_msg(&mut stream))
        .await
        .context("timeout esperando Hello")??;
    let Hello::Control { token_id } = hello;
    // Qualquer erro antes de "Registered" é reportado ao cliente.
    let opener = Opener::new(conn.clone());
    match timeout(
        Duration::from_secs(5),
        negotiate(&mut stream, &state, &opener, &token_id),
    )
    .await
    {
        Ok(Ok((bound, rec))) => run_tunnel(stream, conn, state, bound, rec, opener).await,
        Ok(Err(e)) => {
            state.metrics.handshake_failed();
            reject(&mut stream, &conn, e.to_string()).await;
            Err(e)
        }
        Err(_) => {
            state.metrics.handshake_failed();
            bail!("timeout no handshake")
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
    stream: &mut MuxStream,
    state: &Arc<State>,
    opener: &Opener,
    token_id: &str,
) -> Result<(Bound, TokenRecord)> {
    // Id desconhecido recebe o desafio igual a um id válido e falha só depois
    // da verificação, com a MESMA mensagem: assim não dá para descobrir quais
    // ids existem (user enumeration) olhando respostas ou tempo.
    let record = state.store.find(token_id).await?;
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

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn subdomain_validation() {
        for ok in ["a", "app", "ana-dev", "a1b2c3d4", &"a".repeat(63)] {
            assert!(validate_subdomain(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a.b", "a_b", "ç", &"a".repeat(64)] {
            assert!(validate_subdomain(bad).is_err(), "{bad}");
        }
    }

    const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    #[tokio::test]
    async fn bind_port_rejects_port_outside_range() {
        let err = bind_port(LOCALHOST, &(20000..=20010), Some(80), TcpListener::bind)
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("fora da faixa"), "{err}");
    }

    #[tokio::test]
    async fn bind_port_picks_a_free_port_in_range() {
        let range = 40000..=40100;
        let (_listener, port) = bind_port(LOCALHOST, &range, None, TcpListener::bind)
            .await
            .unwrap();
        assert!(range.contains(&port));
    }

    #[tokio::test]
    async fn bind_port_reports_busy_port() {
        let (_held, port) = bind_port(LOCALHOST, &(41000..=41100), None, TcpListener::bind)
            .await
            .unwrap();
        let err = bind_port(LOCALHOST, &(41000..=41100), Some(port), TcpListener::bind)
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("indisponível"), "{err}");
    }
}
