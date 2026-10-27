//! Protocolo compartilhado entre `hbt-server` e `hbt` (cliente).
//!
//! Este crate é uma *library* (`lib.rs`): não tem `main`, só código que
//! outros crates do workspace importam. Ter o protocolo num lugar só garante
//! que cliente e servidor nunca discordem sobre o formato das mensagens —
//! o compilador checa os dois lados contra os mesmos tipos.

#![warn(missing_docs)]

use std::net::SocketAddr;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

pub mod error;
pub mod mux;

pub use error::ProtoError;

/// Tamanho máximo de um frame. Sem esse limite, um atacante poderia mandar
/// "tamanho = 4 GB" e nos fazer alocar essa memória antes de qualquer
/// validação. 128 KiB cobre um datagrama UDP (máx. ~64 KiB) com folga.
pub const MAX_FRAME: usize = 128 * 1024;

/// Intervalo entre heartbeats e tempo sem ouvir nada até declarar o par morto.
/// Intervalo entre heartbeats.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
/// Tempo sem ouvir nada do par até declará-lo morto.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

/// O que o cliente quer expor. Cada variante carrega só os campos que fazem
/// sentido para ela: o tipo torna impossível, por exemplo, pedir um
/// subdomínio num túnel UDP (em vez de um campo opcional ignorado).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TunnelSpec {
    /// `None` = o servidor sorteia uma porta livre da faixa permitida.
    /// Túnel TCP numa porta pública dedicada.
    Tcp {
        /// Porta pública pedida.
        remote_port: Option<u16>,
    },
    /// Túnel UDP numa porta pública dedicada.
    Udp {
        /// Porta pública pedida.
        remote_port: Option<u16>,
    },
    /// Túnel HTTP: o servidor roteia por `Host: <subdomínio>.<domínio>`.
    /// `None` = servidor sorteia um subdomínio.
    Http {
        /// Subdomínio pedido.
        subdomain: Option<String>,
    },
}

/// Primeira mensagem de TODA conexão que chega na porta de controle.
/// Um `enum` em Rust é uma "union tagged": o valor é exatamente UMA variante,
/// e o `match` obriga a tratar todas.
///
/// O bincode identifica a variante pela POSIÇÃO. Por isso as variantes do
/// protocolo antigo continuam aqui, no mesmo lugar: um cliente antigo ainda
/// é entendido o suficiente para receber "atualize o hbt" em vez de lixo.
#[derive(Debug, Serialize, Deserialize)]
pub enum Hello {
    /// Protocolo 1 (uma conexão TCP nova por usuário). Só é recusado.
    V1Control {
        /// Id do token.
        token_id: String,
    },
    /// Protocolo 1: conexão de dados. Só é recusado.
    V1Accept(u128),
    /// "Sou o cliente cujo token tem este id; abrindo um túnel": segue o
    /// desafio HMAC. O id NÃO é secreto (é só o "nome de usuário"); serve
    /// para o servidor achar qual chave usar na verificação.
    ///
    /// Depois de `Registered`, a conexão vira uma sessão multiplexada
    /// (veja [`mux`]).
    Control {
        /// Id do token (não secreto).
        token_id: String,
    },
}

/// Endereço público do túnel. O servidor não sabe com que nome o mundo o
/// enxerga em TCP/UDP (só o cliente sabe o host que discou), então para esses
/// devolve só a porta; em HTTP devolve a URL completa (ele conhece o domínio).
#[derive(Debug, Serialize, Deserialize)]
pub enum Public {
    /// URL completa (túneis HTTP).
    Url(String),
    /// Porta pública (túneis TCP/UDP).
    Port(u16),
}

/// Mensagens do servidor -> cliente: no handshake e, depois, no stream de controle.
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Desafio de autenticação: nonce de uso único.
    Challenge([u8; 32]),
    /// Autenticação aceita.
    AuthOk,
    /// `public` diz como o túnel ficou acessível.
    Registered {
        /// Endereço público atribuído ao túnel.
        public: Public,
    },
    /// Protocolo 1; não é mais enviado. Fica para não mudar a posição das
    /// variantes seguintes (o cliente antigo precisa decodificar `Error`).
    V1NewConn(u128),
    /// Protocolo 1; idem.
    V1NewUdpStream(u128),
    /// Sinal de vida periódico.
    Heartbeat,
    /// Erro fatal reportado ao cliente, em texto legível.
    Error(String),
}

/// Mensagens do cliente -> servidor: no handshake e, depois, no stream de controle.
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Resposta ao desafio: HMAC do nonce.
    Auth(Vec<u8>),
    /// Pedido de registro do túnel.
    Register(TunnelSpec),
    /// Sinal de vida periódico.
    Heartbeat,
}

/// Um datagrama UDP viajando dentro do stream UDP do túnel.
/// `peer` = quem mandou (lado público); precisamos dele para responder à
/// pessoa certa, já que UDP não tem "conexão".
#[derive(Debug, Serialize, Deserialize)]
pub struct UdpFrame {
    /// Endereço do par público que enviou (e que receberá a resposta).
    pub peer: SocketAddr,
    /// Conteúdo do datagrama.
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------- framing

/// Escreve `[u32 tamanho big-endian][bytes bincode]`.
///
/// TCP é um *fluxo de bytes* sem fronteiras de mensagem: dois `write` podem
/// chegar colados num `read`, ou um `write` partido em dois. Por isso
/// prefixamos o tamanho ("length-prefix framing").
///
/// Genéricos: `W` é qualquer coisa que implemente `AsyncWrite` (TcpStream,
/// metade de um stream, um buffer em memória nos testes...). O compilador
/// gera uma versão da função para cada tipo usado (monomorphization):
/// zero custo em runtime, ao custo de binário um pouco maior.
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), ProtoError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = bincode::serialize(msg).map_err(ProtoError::Encode)?;
    if body.len() > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge {
            len: body.len(),
            max: MAX_FRAME,
        });
    }
    // Um único buffer => um único write_all => menos syscalls e sem risco de
    // outro task intercalar bytes entre o tamanho e o corpo.
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Lê um frame escrito por `write_msg`.
///
/// ATENÇÃO (cancel safety): `read_exact` NÃO é "cancel safe". Se você usar
/// esta função dentro de `tokio::select!` e outro ramo vencer, o future é
/// descartado no meio da leitura e bytes já consumidos se perdem,
/// corrompendo o stream. Aqui só a chamamos em loops dedicados ou com
/// `timeout` seguido de abandono da conexão.
pub async fn read_msg<R, T>(r: &mut R) -> Result<T, ProtoError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(ProtoError::FrameTooLarge { len, max: MAX_FRAME });
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    bincode::deserialize(&body).map_err(ProtoError::Decode)
}

// ------------------------------------------------------------------- auth

type HmacSha256 = Hmac<Sha256>;

/// Um token tem a forma `<id>.<segredo>`: o `id` identifica o cliente (e
/// aparece em listas/logs); o `segredo` (256 bits aleatórios em hex) é o que
/// prova identidade. Separar os dois permite ao servidor achar o registro
/// SEM comparar segredos contra todos os clientes.
pub fn parse_token(token: &str) -> Option<(&str, &str)> {
    let (id, secret) = token.trim().split_once('.')?;
    let id_ok = !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    let secret_ok = secret.len() >= 32 && secret.bytes().all(|b| b.is_ascii_alphanumeric());
    (id_ok && secret_ok).then_some((id, secret))
}

/// Chave de autenticação derivada do segredo: SHA-256("hbt-token-v1:" + segredo).
///
/// O servidor guarda ESTA chave (nunca o segredo cru) e o cliente a recalcula
/// a partir do token. Honestamente: sem TLS isso NÃO protege contra o vazamento
/// do arquivo do servidor, porque quem tem a chave consegue responder ao
/// desafio ("pass-the-hash"). O ganho real vem quando o canal for TLS e o
/// servidor guardar só um hash da chave (próxima etapa). Já deixamos o formato
/// pronto para essa migração e o token em si nunca fica gravado no servidor.
pub fn derive_key(secret: &str) -> [u8; 32] {
    Sha256::digest(format!("hbt-token-v1:{secret}")).into()
}

/// Codifica bytes em hexadecimal minúsculo.
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decodifica hexadecimal; `None` se o tamanho for ímpar ou houver caractere inválido.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Calcula a resposta ao desafio: HMAC-SHA256(chave, nonce).
///
/// Por que HMAC e não mandar o token? O segredo nunca trafega: o servidor
/// manda um nonce (número aleatório de uso único) e o cliente prova que
/// conhece a chave assinando-o. Um espião que capture a resposta não pode
/// reusá-la (o próximo nonce será outro = "replay protection").
/// Limitação: autentica, mas NÃO criptografa o tráfego (veja docs/GUIDE.md).
pub fn auth_response(key: &[u8], nonce: &[u8; 32]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC aceita qualquer tamanho de chave");
    mac.update(nonce);
    mac.finalize().into_bytes().to_vec()
}

/// Verifica em tempo constante (`verify_slice`), para não vazar quantos
/// bytes iniciais batem via diferença de tempo (timing attack).
pub fn verify_auth(key: &[u8], nonce: &[u8; 32], response: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC aceita qualquer tamanho de chave");
    mac.update(nonce);
    mac.verify_slice(response).is_ok()
}

// ------------------------------------------------------------------- util

/// Guarda RAII: ao sair de escopo (`Drop`), aborta o task.
///
/// Em Rust, quando um valor sai de escopo seu `drop` roda automaticamente
/// — por isso não precisamos de `finally`. Aqui usamos isso para garantir
/// que tasks auxiliares (listener público, heartbeats) morrem junto com a
/// sessão, em qualquer caminho de saída (sucesso, erro ou `?`).
/// Sem isso, `tokio::spawn` deixa o task rodando "solto" para sempre.
pub struct AbortOnDrop(pub JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let sent = UdpFrame {
            peer: "1.2.3.4:5".parse().unwrap(),
            data: vec![1, 2, 3],
        };
        write_msg(&mut a, &sent).await.unwrap();
        let got: UdpFrame = read_msg(&mut b).await.unwrap();
        assert_eq!(got.peer, sent.peer);
        assert_eq!(got.data, sent.data);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let err = read_msg::<_, ServerMsg>(&mut b).await.unwrap_err();
        assert!(matches!(err, ProtoError::FrameTooLarge { .. }));
        assert_eq!(io::Error::from(err).kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn auth_accepts_right_key_only() {
        let nonce = [7u8; 32];
        let key = derive_key("segredo");
        let resp = auth_response(&key, &nonce);
        assert!(verify_auth(&key, &nonce, &resp));
        assert!(!verify_auth(&derive_key("outro"), &nonce, &resp));
        assert!(!verify_auth(&key, &[8u8; 32], &resp));
    }

    #[test]
    fn token_parsing() {
        let secret = "a".repeat(64);
        let t = format!("abc123.{secret}");
        assert_eq!(parse_token(&t), Some(("abc123", secret.as_str())));
        assert_eq!(
            parse_token(&format!("  abc123.{secret}\n")),
            Some(("abc123", secret.as_str()))
        );
        assert_eq!(parse_token("sem-ponto"), None);
        assert_eq!(parse_token(".segredo"), None);
        assert_eq!(parse_token("abc.curto"), None);
        assert_eq!(parse_token(&format!("a/b.{secret}")), None);
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex_decode(&hex_encode(&bytes)), Some(bytes.to_vec()));
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
    }
}
