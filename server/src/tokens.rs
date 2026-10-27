//! Tokens por cliente: criação, listagem, revogação e consulta pelo servidor.
//!
//! Armazenamento: um JSON (`<data-dir>/tokens.json`, permissão 0600). Para uma
//! dezena/centena de clientes é o suficiente e fácil de inspecionar; com
//! milhares de tokens ou escritas concorrentes, troque por SQLite.
//!
//! O processo `serve` RELÊ o arquivo a cada autenticação (e a cada heartbeat,
//! para derrubar sessões revogadas), então `token create/revoke` valem na hora,
//! sem reiniciar nem sinal. Troca: um read de arquivo minúsculo por evento, em
//! vez de cache em memória + invalidação (mais código e mais chance de bug).

use std::fs;
use std::io::Write;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use hbt_proto::{derive_key, hex_encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Parte pública do token (`<id>.<segredo>`).
    pub id: String,
    /// Nome humano ("notebook-ana"), único.
    pub name: String,
    /// hex(derive_key(segredo)). O segredo cru nunca é gravado.
    pub key: String,
    /// Segundos desde 1970 (UTC).
    pub created_at: u64,
    /// Limita as portas TCP/UDP públicas que este token pode abrir
    /// (interseção com a faixa global do servidor). `None` = faixa global.
    pub ports: Option<(u16, u16)>,
    /// `None` = qualquer subdomínio livre; `Some(lista)` = somente estes.
    pub subdomains: Option<Vec<String>>,
}

impl TokenRecord {
    /// Faixa efetiva = faixa global ∩ faixa do token. `None` se ficar vazia.
    pub fn effective_ports(&self, global: &RangeInclusive<u16>) -> Option<RangeInclusive<u16>> {
        let (lo, hi) = match self.ports {
            Some((a, b)) => ((*global.start()).max(a), (*global.end()).min(b)),
            None => (*global.start(), *global.end()),
        };
        (lo <= hi).then_some(lo..=hi)
    }
}

#[derive(Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("tokens.json"),
        }
    }

    /// Versão async para o servidor (não bloqueia o runtime do Tokio).
    pub async fn find(&self, id: &str) -> Result<Option<TokenRecord>> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("lendo {}", self.path.display())),
        };
        Ok(parse(&bytes)?.into_iter().find(|r| r.id == id))
    }

    /// Versão síncrona para os comandos de administração (processo curto).
    pub fn load(&self) -> Result<Vec<TokenRecord>> {
        match fs::read(&self.path) {
            Ok(b) => parse(&b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e).with_context(|| format!("lendo {}", self.path.display())),
        }
    }

    /// Grava de forma ATÔMICA: escreve num arquivo temporário e dá `rename`.
    /// O servidor nunca lê um JSON pela metade, mesmo se ler durante a escrita.
    fn save(&self, records: &[TokenRecord]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("criando {}", dir.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600); // já nasce privado
        opts.open(&tmp)?.write_all(&serde_json::to_vec_pretty(records)?)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Cria um token novo e devolve a string COMPLETA (única vez que existe).
    pub fn create(
        &self,
        name: &str,
        ports: Option<RangeInclusive<u16>>,
        subdomains: Option<Vec<String>>,
    ) -> Result<String> {
        if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
            bail!("nome inválido (1 a 64 caracteres, sem controle)");
        }
        if let Some(list) = &subdomains {
            for s in list {
                crate::control::validate_subdomain(s)?;
            }
        }
        let mut records = self.load()?;
        if records.iter().any(|r| r.name == name) {
            bail!("já existe um token chamado '{name}'");
        }
        let id = loop {
            let candidate = format!("{:08x}", rand::random::<u32>());
            if !records.iter().any(|r| r.id == candidate) {
                break candidate;
            }
        };
        let secret = hex_encode(&rand::random::<[u8; 32]>()); // 256 bits
        records.push(TokenRecord {
            id: id.clone(),
            name: name.to_owned(),
            key: hex_encode(&derive_key(&secret)),
            created_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            ports: ports.map(|r| (*r.start(), *r.end())),
            subdomains,
        });
        self.save(&records)?;
        Ok(format!("{id}.{secret}"))
    }

    /// Remove por nome ou id. Sessões ativas caem no próximo heartbeat.
    pub fn revoke(&self, name_or_id: &str) -> Result<TokenRecord> {
        let mut records = self.load()?;
        let pos = records
            .iter()
            .position(|r| r.name == name_or_id || r.id == name_or_id)
            .with_context(|| format!("token '{name_or_id}' não encontrado"))?;
        let removed = records.remove(pos);
        self.save(&records)?;
        Ok(removed)
    }
}

fn parse(bytes: &[u8]) -> Result<Vec<TokenRecord>> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(vec![]);
    }
    serde_json::from_slice(bytes).context("tokens.json inválido")
}

/// Imprime a tabela de `token list`.
pub fn print_table(records: &[TokenRecord]) {
    if records.is_empty() {
        println!("(nenhum token) crie com: hbt-server token create <nome>");
        return;
    }
    println!(
        "{:<10} {:<20} {:<14} {:<24} CRIADO",
        "ID", "NOME", "PORTAS", "SUBDOMÍNIOS"
    );
    for r in records {
        let ports = r
            .ports
            .map_or("(faixa global)".into(), |(a, b)| format!("{a}-{b}"));
        let subs = r.subdomains.as_ref().map_or("(qualquer)".into(), |l| l.join(","));
        println!(
            "{:<10} {:<20} {:<14} {:<24} {}",
            r.id,
            r.name,
            ports,
            subs,
            format_date(r.created_at)
        );
    }
}

/// Segundos Unix -> "AAAA-MM-DD" (algoritmo de Howard Hinnant, sem crate de datas).
pub fn format_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (TokenStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "hbt-tokens-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        (TokenStore::new(&dir), dir)
    }

    #[test]
    fn create_list_revoke() {
        let (s, dir) = store();
        let t = s.create("ana", None, None).unwrap();
        let (id, secret) = hbt_proto::parse_token(&t).expect("token no formato id.segredo");
        let rec = s.load().unwrap().remove(0);
        assert_eq!(rec.id, id);
        assert_eq!(rec.key, hex_encode(&derive_key(secret)));
        assert!(
            !fs::read_to_string(dir.join("tokens.json"))
                .unwrap()
                .contains(secret),
            "segredo cru não pode ser gravado"
        );

        assert!(s.create("ana", None, None).is_err(), "nome duplicado");
        s.revoke("ana").unwrap();
        assert!(s.load().unwrap().is_empty());
        assert!(s.revoke("ana").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn effective_port_range_is_the_intersection() {
        let mk = |ports| TokenRecord {
            id: "x".into(),
            name: "x".into(),
            key: String::new(),
            created_at: 0,
            ports,
            subdomains: None,
        };
        assert_eq!(mk(None).effective_ports(&(100..=200)), Some(100..=200));
        assert_eq!(
            mk(Some((150, 300))).effective_ports(&(100..=200)),
            Some(150..=200)
        );
        assert_eq!(mk(Some((300, 400))).effective_ports(&(100..=200)), None);
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1_700_000_000), "2023-11-14");
    }
}
