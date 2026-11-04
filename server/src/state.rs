//! Estado compartilhado entre todos os tasks do servidor.

use std::collections::HashMap;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};

use hbt_proto::mux::Opener;

use crate::db::Store;
use crate::metrics::Metrics;
use crate::tls::CertStore;

/// Estado compartilhado entre todos os tasks.
///
/// `Arc<T>` = ponteiro com contagem de referências atômica: vários tasks
/// (possivelmente em threads diferentes) são "donos" do mesmo valor, que só
/// é liberado quando o último dono sai. É o jeito de compartilhar dados
/// entre `tokio::spawn`s, que exigem `'static` (não podem emprestar do
/// escopo que os criou).
pub struct State {
    pub store: Store,
    pub tunnel_ip: IpAddr,
    pub port_range: RangeInclusive<u16>,
    /// Configuração do edge HTTP (None = desabilitado).
    pub http: Option<HttpCfg>,
    /// subdomínio -> sessão do cliente dono dele. O edge HTTP usa isto para
    /// abrir um stream até o cliente certo.
    ///
    /// `std::sync::Mutex` e não `tokio::sync::Mutex`: a seção crítica é só
    /// um get/insert num HashMap (nanossegundos, sem `.await` dentro).
    /// O mutex do Tokio é mais caro e só compensa quando você precisa
    /// segurar o lock ATRAVÉS de um `.await`. O compilador te protege: um
    /// guard de std Mutex não é `Send`, então segurá-lo num await dentro de
    /// um `spawn` dá erro de compilação.
    pub routes: Mutex<HashMap<String, Route>>,
    pub metrics: Arc<Metrics>,
    /// token id -> sessões de túnel abertas agora (o painel do site mostra o LED).
    pub online: Mutex<HashMap<String, usize>>,
    /// Teto de túneis simultâneos por token (as portas públicas são finitas).
    pub max_tunnels: usize,
}

impl State {
    /// Marca o token como conectado até o guard cair; `None` se já está no teto.
    pub fn mark_online(self: &Arc<Self>, token_id: &str) -> Option<OnlineGuard> {
        let mut online = self.online.lock().unwrap();
        let n = online.entry(token_id.to_owned()).or_default();
        if *n >= self.max_tunnels {
            if *n == 0 {
                online.remove(token_id);
            }
            return None;
        }
        *n += 1;
        Some(OnlineGuard {
            state: self.clone(),
            token_id: token_id.to_owned(),
        })
    }

    pub fn is_online(&self, token_id: &str) -> bool {
        self.online.lock().unwrap().contains_key(token_id)
    }
}

/// Desmarca o token ao acabar a sessão (RAII, como `RouteGuard`).
pub struct OnlineGuard {
    state: Arc<State>,
    token_id: String,
}

impl Drop for OnlineGuard {
    fn drop(&mut self) {
        let mut online = self.state.online.lock().unwrap();
        if let Some(n) = online.get_mut(&self.token_id) {
            *n -= 1;
            if *n == 0 {
                online.remove(&self.token_id);
            }
        }
    }
}

pub struct HttpCfg {
    pub domain: String,
    /// Porta pública do edge, só para montar a URL mostrada ao usuário.
    pub port: u16,
    /// HTTPS no edge (None = só http).
    pub https: Option<HttpsCfg>,
}

pub struct HttpsCfg {
    /// Porta pública do HTTPS (443 em produção), para a URL e o redirecionamento.
    pub port: u16,
    pub certs: Arc<CertStore>,
}

impl HttpCfg {
    /// Porta do HTTPS quando já há certificado para servir.
    pub fn https_port(&self) -> Option<u16> {
        self.https.as_ref().filter(|h| h.certs.ready()).map(|h| h.port)
    }

    /// URL pública de um subdomínio: https assim que houver certificado.
    pub fn url(&self, sub: &str) -> String {
        let (scheme, port, default) = match self.https_port() {
            Some(p) => ("https", p, 443),
            None => ("http", self.port, 80),
        };
        let port = if port == default {
            String::new()
        } else {
            format!(":{port}")
        };
        format!("{scheme}://{sub}.{}{port}", self.domain)
    }
}

/// Um túnel HTTP no ar: por onde abrir streams até o cliente e de qual conta
/// do site ele é (None = token do admin).
#[derive(Clone)]
pub struct Route {
    pub opener: Opener,
    pub owner: Option<String>,
}

/// Remove a rota HTTP quando o túnel acaba (RAII, como `AbortOnDrop`) e, se o
/// nome era uma reserva da conta, marca o uso (a reserva expira sem uso).
pub struct RouteGuard {
    pub state: Arc<State>,
    pub subdomain: String,
    pub reserved: bool,
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.state.routes.lock().unwrap().remove(&self.subdomain);
        if self.reserved {
            let (store, name) = (self.state.store.clone(), std::mem::take(&mut self.subdomain));
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn_blocking(move || store.touch_reservation(&name));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn online_cap_per_token_frees_on_drop() {
        let state = Arc::new(State {
            store: Store::memory().unwrap(),
            tunnel_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port_range: 20000..=30000,
            http: None,
            routes: Mutex::default(),
            metrics: Arc::default(),
            online: Mutex::default(),
            max_tunnels: 2,
        });
        let a = state.mark_online("t").unwrap();
        let _b = state.mark_online("t").unwrap();
        assert!(state.mark_online("t").is_none());
        assert!(state.mark_online("outro").is_some(), "o teto é por token");
        drop(a);
        assert!(state.mark_online("t").is_some());
    }
}
