//! Erros do protocolo (framing e serialização).

use std::io;

/// Falha ao enviar ou receber uma mensagem do protocolo.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// Erro de E/O no transporte (conexão fechada, reset, timeout do socket...).
    #[error("erro de E/S: {0}")]
    Io(#[from] io::Error),
    /// O frame excede [`MAX_FRAME`](crate::MAX_FRAME); rejeitado antes de alocar memória.
    #[error("frame de {len} bytes excede o limite de {max}")]
    FrameTooLarge {
        /// Tamanho declarado do frame.
        len: usize,
        /// Limite aceito.
        max: usize,
    },
    /// Não foi possível serializar a mensagem.
    #[error("falha ao serializar mensagem: {0}")]
    Encode(#[source] bincode::Error),
    /// Os bytes recebidos não formam uma mensagem válida.
    #[error("mensagem malformada: {0}")]
    Decode(#[source] bincode::Error),
}

impl From<ProtoError> for io::Error {
    fn from(e: ProtoError) -> Self {
        match e {
            ProtoError::Io(e) => e,
            ProtoError::FrameTooLarge { .. } => io::Error::new(io::ErrorKind::InvalidData, e),
            ProtoError::Encode(_) => io::Error::new(io::ErrorKind::InvalidInput, e),
            ProtoError::Decode(_) => io::Error::new(io::ErrorKind::InvalidData, e),
        }
    }
}
