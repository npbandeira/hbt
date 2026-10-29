//! Configuração QUIC compartilhada: TLS 1.3 com certificado autoassinado e
//! *pinning* de fingerprint.
//!
//! Não há CA nem domínio: o servidor gera um certificado na primeira execução
//! e o cliente só aceita o certificado cujo SHA-256 foi configurado
//! (`--fingerprint`). Quem não conhece o fingerprint não consegue se passar
//! pelo servidor, e o tráfego (incluindo o desafio HMAC) vai criptografado.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::hex_encode;

/// Nome (SNI) usado pelo cliente. Não é verificado: quem autentica é o pin.
pub const SERVER_NAME: &str = "hbt";

const ALPN: &[u8] = b"hbt/2";

/// Streams que o par pode abrir ao mesmo tempo. `open_bi` espera vaga quando
/// o limite é atingido, então é o teto de conexões simultâneas por túnel.
pub const MAX_STREAMS: u32 = 1024;

/// SHA-256 do certificado (DER) em hexadecimal minúsculo.
pub fn fingerprint(cert_der: &[u8]) -> String {
    hex_encode(&Sha256::digest(cert_der))
}

/// Gera um certificado autoassinado: devolve `(certificado DER, chave PKCS#8 DER)`.
pub fn self_signed() -> io::Result<(Vec<u8>, Vec<u8>)> {
    let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()]).map_err(io::Error::other)?;
    Ok((ck.cert.der().to_vec(), ck.signing_key.serialize_der()))
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    // Keep-alive mantém o NAT aberto; sem tráfego algum por 30 s a conexão cai.
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    t.max_idle_timeout(Some(
        Duration::from_secs(30).try_into().expect("30 s cabe em VarInt"),
    ));
    t.max_concurrent_bidi_streams(MAX_STREAMS.into());
    t.max_concurrent_uni_streams(0u32.into());
    // Janelas folgadas: o padrão limita um stream a ~1,25 MiB por RTT.
    t.stream_receive_window((8u32 << 20).into());
    t.send_window(16 << 20);
    // O servidor despeja rajadas de datagramas; o padrão (1 MiB) é curto.
    t.datagram_receive_buffer_size(Some(4 << 20));
    t.datagram_send_buffer_size(4 << 20);
    Arc::new(t)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Configuração do servidor a partir do certificado e da chave (DER).
pub fn server_config(cert_der: Vec<u8>, key_der: Vec<u8>) -> io::Result<quinn::ServerConfig> {
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key_der));
    let mut crypto = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der)], key)
        .map_err(io::Error::other)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(crypto).map_err(io::Error::other)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
    cfg.transport_config(transport());
    Ok(cfg)
}

/// Configuração do cliente: só aceita o servidor cujo certificado tem este fingerprint.
pub fn client_config(fingerprint: &str) -> io::Result<quinn::ClientConfig> {
    let provider = provider();
    let verifier = Pinned {
        fingerprint: fingerprint.trim().replace(':', "").to_ascii_lowercase(),
        provider: provider.clone(),
    };
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(crypto).map_err(io::Error::other)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
    cfg.transport_config(transport());
    Ok(cfg)
}

/// Verificador que troca a cadeia de CAs pela comparação com o fingerprint
/// configurado. As assinaturas do handshake continuam sendo checadas.
#[derive(Debug)]
struct Pinned {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "o fingerprint do servidor não confere com o configurado".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
