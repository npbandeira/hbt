//! Tipo do token e a tabela do `hbt-server token list`.
//!
//! O armazenamento mora em `db.rs` (SQLite, `<data-dir>/hbt.db`).

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

/// Um token no banco. Também é o formato de cada item do `tokens.json` antigo,
/// que `db.rs` importa na primeira abertura.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Parte pública do token (`<id>.<segredo>`).
    pub id: String,
    /// Nome humano ("notebook-ana"), único por dono.
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
    /// Id da conta do site que criou o token; `None` = criado pelo admin no CLI.
    #[serde(default)]
    pub owner: Option<String>,
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

/// Imprime a tabela de `token list`.
pub fn print_table(records: &[TokenRecord]) {
    if records.is_empty() {
        println!("(nenhum token) crie com: hbt-server token create <nome>");
        return;
    }
    println!(
        "{:<10} {:<20} {:<14} {:<24} {:<18} CRIADO",
        "ID", "NOME", "PORTAS", "SUBDOMÍNIOS", "DONO"
    );
    for r in records {
        let ports = r
            .ports
            .map_or("(faixa global)".into(), |(a, b)| format!("{a}-{b}"));
        let subs = r.subdomains.as_ref().map_or("(qualquer)".into(), |l| l.join(","));
        println!(
            "{:<10} {:<20} {:<14} {:<24} {:<18} {}",
            r.id,
            r.name,
            ports,
            subs,
            r.owner.as_deref().unwrap_or("(admin)"),
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

    #[test]
    fn effective_port_range_is_the_intersection() {
        let mk = |ports| TokenRecord {
            id: "x".into(),
            name: "x".into(),
            key: String::new(),
            created_at: 0,
            ports,
            subdomains: None,
            owner: None,
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

    #[test]
    fn old_tokens_json_without_owner_still_parses() {
        let old = r#"[{"id":"a1b2c3d4","name":"n","key":"k","created_at":1,"ports":null,"subdomains":null}]"#;
        let v: Vec<TokenRecord> = serde_json::from_str(old).unwrap();
        assert_eq!(v[0].owner, None);
    }
}
