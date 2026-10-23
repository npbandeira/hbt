//! Protocolo compartilhado entre `hbt-server` e `hbt` (cliente).
//!
//! Este crate é uma *library* (`lib.rs`): não tem `main`, só código que
//! outros crates do workspace importam. Ter o protocolo num lugar só garante
//! que cliente e servidor nunca discordem sobre o formato das mensagens —
//! o compilador checa os dois lados contra os mesmos tipos.

use std::io;
use std::net::SocketAddr;

use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

/// Tamanho máximo de um frame. Sem esse limite, um atacante poderia mandar
/// "tamanho = 4 GB" e nos fazer alocar essa memória antes de qualquer
/// validação. 128 KiB cobre um datagrama UDP (máx. ~64 KiB) com folga.
pub const MAX_FRAME: usize = 128 * 1024;

/// Intervalo entre heartbeats e tempo sem ouvir nada até declarar o par morto.
pub const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
pub const HEARTBEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Identificador de uma conexão pendente. 128 bits aleatórios: impossível de
/// adivinhar, então saber o id funciona como uma "capability" (prova de que
/// você recebeu o id pelo canal de controle já autenticado).
pub type ConnId = u128;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

/// Primeira mensagem de TODA conexão que chega na porta de controle.
/// Um `enum` em Rust é uma "union tagged": o valor é exatamente UMA variante,
/// e o `match` obriga a tratar todas.
#[derive(Debug, Serialize, Deserialize)]
pub enum Hello {
    /// "Sou um cliente abrindo um túnel": segue o desafio HMAC.
    Control,
    /// "Sou uma conexão de dados que o servidor pediu via `NewConn`/`NewUdpStream`".
    Accept(ConnId),
}

/// Mensagens do servidor -> cliente no canal de controle.
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Challenge([u8; 32]),
    AuthOk,
    Registered {
        remote_port: u16,
    },
    /// Chegou um usuário público (TCP): abra uma conexão de dados com este id.
    NewConn(ConnId),
    /// Túnel UDP: abra a conexão de dados que carregará os datagramas.
    NewUdpStream(ConnId),
    Heartbeat,
    Error(String),
}

/// Mensagens do cliente -> servidor no canal de controle.
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    Auth(Vec<u8>),
    Register { proto: Proto, remote_port: u16 },
    Heartbeat,
}

/// Um datagrama UDP viajando dentro do stream TCP de dados.
/// `peer` = quem mandou (lado público); precisamos dele para responder à
/// pessoa certa, já que UDP não tem "conexão".
#[derive(Debug, Serialize, Deserialize)]
pub struct UdpFrame {
    pub peer: SocketAddr,
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
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = bincode::serialize(msg).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    // Um único buffer => um único write_all => menos syscalls e sem risco de
    // outro task intercalar bytes entre o tamanho e o corpo.
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await
}

/// Lê um frame escrito por `write_msg`.
///
/// ATENÇÃO (cancel safety): `read_exact` NÃO é "cancel safe". Se você usar
/// esta função dentro de `tokio::select!` e outro ramo vencer, o future é
/// descartado no meio da leitura e bytes já consumidos se perdem,
/// corrompendo o stream. Aqui só a chamamos em loops dedicados ou com
/// `timeout` seguido de abandono da conexão.
pub async fn read_msg<R, T>(r: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    bincode::deserialize(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ------------------------------------------------------------------- auth

type HmacSha256 = Hmac<Sha256>;

/// Calcula a resposta ao desafio: HMAC-SHA256(segredo, nonce).
///
/// Por que HMAC e não mandar o token? O segredo nunca trafega: o servidor
/// manda um nonce (número aleatório de uso único) e o cliente prova que
/// conhece o segredo assinando-o. Um espião que capture a resposta não pode
/// reusá-la (o próximo nonce será outro = "replay protection").
/// Limitação: autentica, mas NÃO criptografa o tráfego (veja docs/GUIDE.md).
pub fn auth_response(secret: &str, nonce: &[u8; 32]) -> Vec<u8> {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC aceita qualquer tamanho de chave");
    mac.update(nonce);
    mac.finalize().into_bytes().to_vec()
}

/// Verifica em tempo constante (`verify_slice`), para não vazar quantos
/// bytes iniciais batem via diferença de tempo (timing attack).
pub fn verify_auth(secret: &str, nonce: &[u8; 32], response: &[u8]) -> bool {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC aceita qualquer tamanho de chave");
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
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn auth_accepts_right_secret_only() {
        let nonce = [7u8; 32];
        let resp = auth_response("segredo", &nonce);
        assert!(verify_auth("segredo", &nonce, &resp));
        assert!(!verify_auth("outro", &nonce, &resp));
        assert!(!verify_auth("segredo", &[8u8; 32], &resp));
    }
}
