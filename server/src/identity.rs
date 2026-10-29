//! Identidade TLS do servidor: certificado autoassinado criado na primeira
//! execução e reaproveitado depois (`<data-dir>/cert.der` e `key.der`).
//!
//! O fingerprint do certificado é o que o cliente confere (`--fingerprint`);
//! apagar os arquivos gera outra identidade e exige reconfigurar os clientes.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use hbt_proto::quic;

pub struct Identity {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

impl Identity {
    pub fn fingerprint(&self) -> String {
        quic::fingerprint(&self.cert)
    }
}

pub fn load_or_create(data_dir: &Path) -> Result<Identity> {
    let (cert_path, key_path) = (data_dir.join("cert.der"), data_dir.join("key.der"));
    if let (Ok(cert), Ok(key)) = (fs::read(&cert_path), fs::read(&key_path)) {
        return Ok(Identity { cert, key });
    }
    fs::create_dir_all(data_dir).with_context(|| format!("criando {}", data_dir.display()))?;
    let (cert, key) = quic::self_signed()?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600); // a chave já nasce privada
    opts.open(&key_path)
        .and_then(|mut f| f.write_all(&key))
        .with_context(|| format!("gravando {}", key_path.display()))?;
    fs::write(&cert_path, &cert).with_context(|| format!("gravando {}", cert_path.display()))?;
    Ok(Identity { cert, key })
}
