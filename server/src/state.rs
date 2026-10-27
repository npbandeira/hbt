//! Estado compartilhado entre todos os tasks do servidor.

use std::collections::HashMap;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};

use hbt_proto::mux::Opener;

use crate::metrics::Metrics;
use crate::tokens::TokenStore;

/// Estado compartilhado entre todos os tasks.
///
/// `Arc<T>` = ponteiro com contagem de referências atômica: vários tasks
/// (possivelmente em threads diferentes) são "donos" do mesmo valor, que só
/// é liberado quando o último dono sai. É o jeito de compartilhar dados
/// entre `tokio::spawn`s, que exigem `'static` (não podem emprestar do
/// escopo que os criou).
pub struct State {
    pub tokens: TokenStore,
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
    pub routes: Mutex<HashMap<String, Opener>>,
    pub metrics: Arc<Metrics>,
}

pub struct HttpCfg {
    pub domain: String,
    /// Porta pública do edge, só para montar a URL mostrada ao usuário.
    pub port: u16,
}

/// Remove a rota HTTP quando o túnel acaba (RAII, como `AbortOnDrop`).
pub struct RouteGuard {
    pub state: Arc<State>,
    pub subdomain: String,
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.state.routes.lock().unwrap().remove(&self.subdomain);
    }
}
