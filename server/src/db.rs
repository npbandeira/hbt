//! Armazenamento em SQLite (`<data-dir>/hbt.db`): tokens e contas do site.
//!
//! Por que SQLite e não um JSON: o servidor consulta o token a cada handshake
//! e a cada heartbeat de cada sessão (10 s), e dois processos escrevem aqui
//! (o servidor, pelo site, e o `hbt-server token|user ...` do admin). Com um
//! arquivo JSON cada consulta relia e reparseava tudo, e duas escritas
//! simultâneas perdiam uma. Aqui a consulta é um índice e o WAL com
//! `busy_timeout` serializa as escritas entre processos.
//!
//! A conexão é síncrona atrás de um `Mutex`; o código assíncrono usa
//! [`Store::find`] (que roda em `spawn_blocking`) para não parar o Tokio.

use std::fs;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use hbt_proto::{derive_key, hex_encode};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};

use crate::tokens::TokenRecord;

/// Erros que quem chama precisa distinguir (via `downcast_ref`).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("já existe um token chamado '{0}'")]
    DuplicateName(String),
    #[error("já existe uma conta com este e-mail")]
    DuplicateEmail,
    #[error("limite de {0} tokens atingido")]
    Quota(usize),
    #[error("{0} não encontrado")]
    NotFound(String),
    #[error("'{0}' é ambíguo: use o id")]
    Ambiguous(String),
}

/// Uma conta do site.
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id: String,
    pub email: String,
    pub password_hash: String,
    pub created_at: u64,
    pub disabled: bool,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tokens (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    key         TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    port_lo     INTEGER,
    port_hi     INTEGER,
    subdomains  TEXT,
    owner       TEXT
);
CREATE INDEX IF NOT EXISTS tokens_owner ON tokens(owner);
-- Nome único por dono; os tokens do admin (owner NULL) dividem um espaço só.
CREATE UNIQUE INDEX IF NOT EXISTS tokens_owner_name ON tokens(COALESCE(owner, ''), name);
CREATE TABLE IF NOT EXISTS users (
    id             TEXT PRIMARY KEY,
    email          TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash  TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    disabled       INTEGER NOT NULL DEFAULT 0
);
";

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// SQLite só guarda inteiros com sinal; segundos Unix cabem de sobra.
fn secs_i64(t: u64) -> i64 {
    i64::try_from(t).unwrap_or(i64::MAX)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Violação de `UNIQUE`/`PRIMARY KEY` do SQLite.
fn is_unique(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation)
}

/// Repete `f` por até ~5 s enquanto o SQLite responder "ocupado".
fn retry_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let mut tries = 0;
    loop {
        match f() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && tries < 100 =>
            {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

fn token_row(r: &Row<'_>) -> rusqlite::Result<TokenRecord> {
    let lo: Option<u16> = r.get("port_lo")?;
    let hi: Option<u16> = r.get("port_hi")?;
    let subs: Option<String> = r.get("subdomains")?;
    Ok(TokenRecord {
        id: r.get("id")?,
        name: r.get("name")?,
        key: r.get("key")?,
        created_at: r.get::<_, i64>("created_at")?.try_into().unwrap_or(0),
        ports: lo.zip(hi),
        // Texto gravado por nós mesmos (JSON de uma lista); ilegível = sem lista.
        subdomains: subs.and_then(|s| serde_json::from_str(&s).ok()),
        owner: r.get("owner")?,
    })
}

fn user_row(r: &Row<'_>) -> rusqlite::Result<UserRecord> {
    Ok(UserRecord {
        id: r.get("id")?,
        email: r.get("email")?,
        password_hash: r.get("password_hash")?,
        created_at: r.get::<_, i64>("created_at")?.try_into().unwrap_or(0),
        disabled: r.get::<_, i64>("disabled")? != 0,
    })
}

impl Store {
    /// Abre (ou cria) `<dir>/hbt.db` e importa o `tokens.json` antigo, se houver.
    pub fn open(dir: &Path) -> Result<Store> {
        fs::create_dir_all(dir).with_context(|| format!("criando {}", dir.display()))?;
        let path = dir.join("hbt.db");
        if !path.exists() {
            // Já nasce privado; o SQLite cria -wal/-shm com a mesma permissão.
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(false);
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            opts.open(&path)
                .with_context(|| format!("criando {}", path.display()))?;
        }
        let conn = Connection::open(&path).with_context(|| format!("abrindo {}", path.display()))?;
        let store = Store::init(conn)?;
        store.import_json(&dir.join("tokens.json"))?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.busy_timeout(Duration::from_secs(5))?;
        // Num arquivo novo, trocar para WAL e criar as tabelas de dois processos ao
        // mesmo tempo pode dar BUSY na hora (o SQLite recusa para evitar deadlock, sem
        // esperar o `busy_timeout`). Depois da primeira vez o modo WAL fica gravado no
        // arquivo e isso não acontece mais, então tentar de novo aqui basta.
        retry_busy(|| conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;"))?;
        retry_busy(|| conn.execute_batch(SCHEMA))?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        // Um panic noutra thread não deixa o banco inconsistente (as escritas
        // são transações), então seguimos com a conexão mesmo envenenada.
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }

    /// Roda uma consulta síncrona fora dos workers do Tokio.
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || f(&store)).await?
    }

    /// Importa o `tokens.json` da versão anterior (uma vez) e o renomeia para
    /// `tokens.json.migrated`. Os tokens continuam valendo: a `key` já é o hash.
    fn import_json(&self, json: &Path) -> Result<()> {
        let Ok(bytes) = fs::read(json) else {
            return Ok(());
        };
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let empty = tx.query_row("SELECT COUNT(*) FROM tokens", [], |r| r.get::<_, i64>(0))? == 0;
            if !empty {
                return Ok(());
            }
            let records: Vec<TokenRecord> = if bytes.iter().all(u8::is_ascii_whitespace) {
                vec![]
            } else {
                serde_json::from_slice(&bytes).context("tokens.json inválido")?
            };
            for r in &records {
                insert_token(&tx, r)?;
            }
            tx.commit()?;
            fs::rename(json, json.with_extension("json.migrated"))?;
            tracing::info!("{} tokens importados de {}", records.len(), json.display());
            Ok(())
        })
    }

    // ------------------------------------------------------------- tokens

    pub fn get(&self, id: &str) -> Result<Option<TokenRecord>> {
        self.with(|c| {
            Ok(c.query_row("SELECT * FROM tokens WHERE id = ?1", [id], token_row)
                .optional()?)
        })
    }

    /// Versão assíncrona de [`Store::get`] para o servidor.
    pub async fn find(&self, id: &str) -> Result<Option<TokenRecord>> {
        let id = id.to_owned();
        self.run(move |s| s.get(&id)).await
    }

    pub fn load(&self) -> Result<Vec<TokenRecord>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM tokens ORDER BY created_at, name")?;
            let rows = st.query_map([], token_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn tokens_of(&self, owner: &str) -> Result<Vec<TokenRecord>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM tokens WHERE owner = ?1 ORDER BY created_at, name")?;
            let rows = st.query_map([owner], token_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Cria um token e devolve a string COMPLETA `id.segredo` (a única vez que
    /// o segredo existe). `owner`/`max` aplicam a cota por conta do site, dentro
    /// da mesma transação da inserção (duas requisições não furam o limite).
    pub fn create(
        &self,
        name: &str,
        ports: Option<RangeInclusive<u16>>,
        subdomains: Option<Vec<String>>,
        owner: Option<&str>,
        max: Option<usize>,
    ) -> Result<String> {
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            anyhow::bail!("nome inválido (1 a 64 caracteres, sem controle)");
        }
        if let Some(list) = &subdomains {
            for s in list {
                crate::control::validate_subdomain(s)?;
            }
        }
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let (Some(owner), Some(max)) = (owner, max) {
                let n: i64 = tx.query_row("SELECT COUNT(*) FROM tokens WHERE owner = ?1", [owner], |r| {
                    r.get(0)
                })?;
                if usize::try_from(n).unwrap_or(usize::MAX) >= max {
                    return Err(StoreError::Quota(max).into());
                }
            }
            let id = loop {
                let candidate = format!("{:08x}", rand::random::<u32>());
                let taken: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tokens WHERE id = ?1)",
                    [&candidate],
                    |r| r.get(0),
                )?;
                if !taken {
                    break candidate;
                }
            };
            let secret = hex_encode(&rand::random::<[u8; 32]>()); // 256 bits
            let rec = TokenRecord {
                id: id.clone(),
                name: name.to_owned(),
                key: hex_encode(&derive_key(&secret)),
                created_at: now(),
                ports: ports.map(|r| (*r.start(), *r.end())),
                subdomains,
                owner: owner.map(str::to_owned),
            };
            match insert_token(&tx, &rec) {
                Err(e) if e.downcast_ref::<rusqlite::Error>().is_some_and(is_unique) => {
                    return Err(StoreError::DuplicateName(name.to_owned()).into())
                }
                other => other?,
            }
            tx.commit()?;
            Ok(format!("{id}.{secret}"))
        })
    }

    /// Remove por id ou por nome (CLI do admin). Sessões ativas caem no próximo
    /// heartbeat. Nome repetido entre contas pede o id.
    pub fn revoke(&self, name_or_id: &str) -> Result<TokenRecord> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let found: Vec<TokenRecord> = {
                let mut st = tx.prepare("SELECT * FROM tokens WHERE id = ?1 OR name = ?1")?;
                let rows = st.query_map([name_or_id], token_row)?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let rec = match found.as_slice() {
                [] => return Err(StoreError::NotFound(format!("token '{name_or_id}'")).into()),
                [one] => one.clone(),
                _ => match found.iter().find(|r| r.id == name_or_id) {
                    Some(by_id) => by_id.clone(),
                    None => return Err(StoreError::Ambiguous(name_or_id.to_owned()).into()),
                },
            };
            tx.execute("DELETE FROM tokens WHERE id = ?1", [&rec.id])?;
            tx.commit()?;
            Ok(rec)
        })
    }

    /// Revoga só se o token for da conta (o site nunca apaga token alheio).
    pub fn revoke_owned(&self, owner: &str, id: &str) -> Result<bool> {
        self.with(|c| Ok(c.execute("DELETE FROM tokens WHERE id = ?1 AND owner = ?2", [id, owner])? > 0))
    }

    // -------------------------------------------------------------- contas

    pub fn add_user(&self, email: &str, password_hash: &str) -> Result<UserRecord> {
        let rec = UserRecord {
            id: hex_encode(&rand::random::<[u8; 8]>()),
            email: email.trim().to_lowercase(),
            password_hash: password_hash.to_owned(),
            created_at: now(),
            disabled: false,
        };
        self.with(|c| {
            match c.execute(
                "INSERT INTO users (id, email, password_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![rec.id, rec.email, rec.password_hash, secs_i64(rec.created_at)],
            ) {
                Err(e) if is_unique(&e) => Err(StoreError::DuplicateEmail.into()),
                other => Ok(other.map(|_| ())?),
            }
        })?;
        Ok(rec)
    }

    pub fn user_by_id(&self, id: &str) -> Result<Option<UserRecord>> {
        self.with(|c| {
            Ok(c.query_row("SELECT * FROM users WHERE id = ?1", [id], user_row)
                .optional()?)
        })
    }

    pub fn user_by_email(&self, email: &str) -> Result<Option<UserRecord>> {
        self.with(|c| {
            Ok(
                c.query_row("SELECT * FROM users WHERE email = ?1", [email.trim()], user_row)
                    .optional()?,
            )
        })
    }

    /// Conta por id ou e-mail (CLI do admin).
    fn user_by_ident(c: &Connection, ident: &str) -> Result<UserRecord> {
        c.query_row(
            "SELECT * FROM users WHERE id = ?1 OR email = ?1",
            [ident.trim()],
            user_row,
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("conta '{ident}'")).into())
    }

    /// Contas com o número de tokens de cada uma.
    pub fn list_users(&self) -> Result<Vec<(UserRecord, usize)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT u.*, (SELECT COUNT(*) FROM tokens t WHERE t.owner = u.id) AS n
                 FROM users u ORDER BY u.created_at, u.email",
            )?;
            let rows = st.query_map([], |r| {
                Ok((user_row(r)?, usize::try_from(r.get::<_, i64>("n")?).unwrap_or(0)))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Desativa (e revoga todos os tokens da conta, na mesma transação) ou reativa.
    pub fn set_disabled(&self, ident: &str, disabled: bool) -> Result<(UserRecord, usize)> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut user = Store::user_by_ident(&tx, ident)?;
            tx.execute(
                "UPDATE users SET disabled = ?2 WHERE id = ?1",
                params![user.id, disabled],
            )?;
            let revoked = if disabled {
                tx.execute("DELETE FROM tokens WHERE owner = ?1", [&user.id])?
            } else {
                0
            };
            tx.commit()?;
            user.disabled = disabled;
            Ok((user, revoked))
        })
    }

    pub fn set_password(&self, ident: &str, password_hash: &str) -> Result<UserRecord> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let user = Store::user_by_ident(&tx, ident)?;
            tx.execute(
                "UPDATE users SET password_hash = ?2 WHERE id = ?1",
                params![user.id, password_hash],
            )?;
            tx.commit()?;
            Ok(user)
        })
    }

    /// Apaga a conta e os tokens dela.
    pub fn remove_user(&self, ident: &str) -> Result<(UserRecord, usize)> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let user = Store::user_by_ident(&tx, ident)?;
            let revoked = tx.execute("DELETE FROM tokens WHERE owner = ?1", [&user.id])?;
            tx.execute("DELETE FROM users WHERE id = ?1", [&user.id])?;
            tx.commit()?;
            Ok((user, revoked))
        })
    }
}

fn insert_token(c: &Connection, r: &TokenRecord) -> Result<()> {
    c.execute(
        "INSERT INTO tokens (id, name, key, created_at, port_lo, port_hi, subdomains, owner)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            r.id,
            r.name,
            r.key,
            secs_i64(r.created_at),
            r.ports.map(|p| p.0),
            r.ports.map(|p| p.1),
            r.subdomains.as_ref().map(serde_json::to_string).transpose()?,
            r.owner,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_of<T: std::fmt::Debug>(r: Result<T>) -> StoreError {
        match r.unwrap_err().downcast::<StoreError>() {
            Ok(e) => e,
            Err(e) => panic!("erro inesperado: {e:#}"),
        }
    }

    #[test]
    fn create_get_list_revoke() {
        let s = Store::memory().unwrap();
        let t = s.create("ana", None, None, None, None).unwrap();
        let (id, secret) = hbt_proto::parse_token(&t).expect("token no formato id.segredo");
        let rec = s.get(id).unwrap().unwrap();
        assert_eq!(rec.name, "ana");
        assert_eq!(rec.key, hex_encode(&derive_key(secret)));
        assert!(!rec.key.contains(secret), "segredo cru não pode ser gravado");
        assert_eq!(s.load().unwrap().len(), 1);

        assert!(matches!(
            err_of(s.create("ana", None, None, None, None)),
            StoreError::DuplicateName(_)
        ));
        s.revoke("ana").unwrap();
        assert!(s.load().unwrap().is_empty());
        assert!(matches!(err_of(s.revoke("ana")), StoreError::NotFound(_)));
    }

    #[test]
    fn ports_and_subdomains_roundtrip() {
        let s = Store::memory().unwrap();
        let t = s
            .create(
                "x",
                Some(20000..=20010),
                Some(vec!["a".into(), "b-c".into()]),
                None,
                None,
            )
            .unwrap();
        let rec = s.get(t.split_once('.').unwrap().0).unwrap().unwrap();
        assert_eq!(rec.ports, Some((20000, 20010)));
        assert_eq!(rec.subdomains, Some(vec!["a".into(), "b-c".into()]));
        assert!(s.create("y", None, Some(vec!["A_".into()]), None, None).is_err());
    }

    #[test]
    fn names_are_unique_per_owner_and_quota_is_enforced() {
        let s = Store::memory().unwrap();
        let ana = s.add_user("Ana@x.com", "h").unwrap();
        let bia = s.add_user("bia@x.com", "h").unwrap();
        s.create("notebook", None, None, Some(&ana.id), Some(2)).unwrap();
        s.create("notebook", None, None, Some(&bia.id), Some(2)).unwrap(); // outro dono: ok
        assert!(matches!(
            err_of(s.create("notebook", None, None, Some(&ana.id), Some(2))),
            StoreError::DuplicateName(_)
        ));
        s.create("casa", None, None, Some(&ana.id), Some(2)).unwrap();
        assert!(matches!(
            err_of(s.create("extra", None, None, Some(&ana.id), Some(2))),
            StoreError::Quota(2)
        ));
        assert_eq!(s.tokens_of(&ana.id).unwrap().len(), 2);
        assert_eq!(s.tokens_of(&bia.id).unwrap().len(), 1);
        // Nome repetido entre contas: o CLI exige o id.
        assert!(matches!(err_of(s.revoke("notebook")), StoreError::Ambiguous(_)));
    }

    #[test]
    fn revoke_owned_refuses_foreign_tokens() {
        let s = Store::memory().unwrap();
        let ana = s.add_user("ana@x.com", "h").unwrap();
        let bia = s.add_user("bia@x.com", "h").unwrap();
        let t = s.create("t", None, None, Some(&ana.id), None).unwrap();
        let id = t.split_once('.').unwrap().0;
        assert!(!s.revoke_owned(&bia.id, id).unwrap());
        assert!(s.get(id).unwrap().is_some());
        assert!(s.revoke_owned(&ana.id, id).unwrap());
        assert!(s.get(id).unwrap().is_none());
    }

    #[test]
    fn duplicate_email_ignores_case() {
        let s = Store::memory().unwrap();
        s.add_user("Ana@X.com", "h").unwrap();
        assert!(matches!(
            err_of(s.add_user("ana@x.COM", "h")),
            StoreError::DuplicateEmail
        ));
        assert!(s.user_by_email("ANA@x.com").unwrap().is_some());
    }

    #[test]
    fn disabling_a_user_revokes_all_their_tokens() {
        let s = Store::memory().unwrap();
        let ana = s.add_user("ana@x.com", "h").unwrap();
        let admin_token = s.create("admin", None, None, None, None).unwrap();
        s.create("a", None, None, Some(&ana.id), None).unwrap();
        s.create("b", None, None, Some(&ana.id), None).unwrap();
        let (u, revoked) = s.set_disabled("ana@x.com", true).unwrap();
        assert!(u.disabled);
        assert_eq!(revoked, 2);
        assert!(s.user_by_id(&ana.id).unwrap().unwrap().disabled);
        assert!(s.tokens_of(&ana.id).unwrap().is_empty());
        let admin_id = admin_token.split_once('.').unwrap().0;
        assert!(s.get(admin_id).unwrap().is_some(), "token do admin não é tocado");
        s.set_disabled(&ana.id, false).unwrap();
        assert!(!s.user_by_id(&ana.id).unwrap().unwrap().disabled);
    }

    #[test]
    fn removing_a_user_removes_their_tokens() {
        let s = Store::memory().unwrap();
        let ana = s.add_user("ana@x.com", "h").unwrap();
        s.create("a", None, None, Some(&ana.id), None).unwrap();
        let (_, n) = s.remove_user("ana@x.com").unwrap();
        assert_eq!(n, 1);
        assert!(s.load().unwrap().is_empty());
        assert!(matches!(
            err_of(s.remove_user("ana@x.com")),
            StoreError::NotFound(_)
        ));
    }

    #[test]
    fn list_users_counts_tokens() {
        let s = Store::memory().unwrap();
        let ana = s.add_user("ana@x.com", "h").unwrap();
        s.add_user("bia@x.com", "h").unwrap();
        s.create("a", None, None, Some(&ana.id), None).unwrap();
        let l = s.list_users().unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l.iter().find(|(u, _)| u.email == "ana@x.com").unwrap().1, 1);
    }

    #[test]
    fn tokens_json_is_imported_once_and_old_tokens_keep_working() {
        let dir =
            std::env::temp_dir().join(format!("hbt-db-{}-{}", std::process::id(), rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        let secret = "s".repeat(64);
        let key = hex_encode(&derive_key(&secret));
        let json = format!(
            r#"[{{"id":"a1b2c3d4","name":"antigo","key":"{key}","created_at":1700000000,"ports":[20000,20010],"subdomains":["ana"]}}]"#
        );
        fs::write(dir.join("tokens.json"), json).unwrap();

        let s = Store::open(&dir).unwrap();
        let rec = s.get("a1b2c3d4").unwrap().expect("token importado");
        assert_eq!(rec.key, key, "a chave derivada é a mesma: o token segue valendo");
        assert_eq!(rec.ports, Some((20000, 20010)));
        assert_eq!(rec.subdomains, Some(vec!["ana".into()]));
        assert!(!dir.join("tokens.json").exists());
        assert!(dir.join("tokens.json.migrated").exists());

        // Reabrir não importa de novo nem perde nada.
        drop(s);
        assert_eq!(Store::open(&dir).unwrap().load().unwrap().len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    /// Regressão: abrir um banco NOVO de vários processos ao mesmo tempo (o servidor
    /// subindo e o admin rodando `token create` logo em seguida) dava "database is
    /// locked": trocar para WAL e criar as tabelas pode falhar na hora, sem esperar
    /// o `busy_timeout`.
    #[test]
    fn opening_a_fresh_database_concurrently_never_fails() {
        for round in 0..40 {
            let dir = std::env::temp_dir().join(format!(
                "hbt-db-{}-{round}-{}",
                std::process::id(),
                rand::random::<u32>()
            ));
            let barrier = Arc::new(std::sync::Barrier::new(6));
            let opens: Vec<_> = (0..6)
                .map(|_| {
                    let (dir, barrier) = (dir.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        Store::open(&dir).map(|_| ())
                    })
                })
                .collect();
            for o in opens {
                o.join()
                    .unwrap()
                    .unwrap_or_else(|e| panic!("rodada {round}: {e:#}"));
            }
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn two_connections_on_the_same_file_do_not_lose_writes() {
        let dir =
            std::env::temp_dir().join(format!("hbt-db-{}-{}", std::process::id(), rand::random::<u32>()));
        let (a, b) = (Store::open(&dir).unwrap(), Store::open(&dir).unwrap());
        let threads: Vec<_> = [a.clone(), b]
            .into_iter()
            .enumerate()
            .map(|(k, s)| {
                std::thread::spawn(move || {
                    for i in 0..30 {
                        s.create(&format!("t{k}-{i}"), None, None, None, None).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(a.load().unwrap().len(), 60, "nenhuma escrita perdida");
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Medição (não é teste de regressão): `cargo test -p hbt-server --release -- --ignored --nocapture heartbeat_lookup`.
    /// Compara a consulta por `id` no SQLite com reler e reparsear um `tokens.json`
    /// do mesmo tamanho, que era o que cada heartbeat fazia antes.
    #[test]
    #[ignore = "benchmark"]
    fn heartbeat_lookup_sqlite_vs_json() {
        let dir = std::env::temp_dir().join(format!("hbt-bench-{}", std::process::id()));
        let s = Store::open(&dir).unwrap();
        let n = 10_000;
        s.with(|c| {
            let tx = c.transaction()?;
            for i in 0..n {
                let rec = TokenRecord {
                    id: format!("{i:08x}"),
                    name: format!("token-{i}"),
                    key: hex_encode(&derive_key(&i.to_string())),
                    created_at: 1_700_000_000,
                    ports: None,
                    subdomains: None,
                    owner: Some(format!("{:016x}", i % 500)),
                };
                insert_token(&tx, &rec)?;
            }
            tx.commit()?;
            Ok(())
        })
        .unwrap();

        let json = serde_json::to_vec_pretty(&s.load().unwrap()).unwrap();
        let path = dir.join("bench.json");
        fs::write(&path, &json).unwrap();

        let rounds = 200;
        let t = std::time::Instant::now();
        for i in 0..rounds {
            let id = format!("{:08x}", (i * 37) % n);
            assert!(s.get(&id).unwrap().is_some());
        }
        let sqlite = t.elapsed() / rounds;

        let t = std::time::Instant::now();
        for i in 0..rounds {
            let id = format!("{:08x}", (i * 37) % n);
            let all: Vec<TokenRecord> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert!(all.iter().any(|r| r.id == id));
        }
        let json_each = t.elapsed() / rounds;

        println!(
            "{n} tokens ({} KB de JSON): SQLite {sqlite:?} por consulta | JSON relido {json_each:?} por consulta ({:.0}x)",
            json.len() / 1024,
            json_each.as_secs_f64() / sqlite.as_secs_f64()
        );
        let _ = fs::remove_dir_all(dir);
    }
}
