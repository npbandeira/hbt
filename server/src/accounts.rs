//! Senhas das contas do site: argon2id, geração de senha e validação de e-mail.

use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use rand::distributions::Alphanumeric;
use rand::Rng;

/// Tamanho mínimo de uma senha escolhida no site.
pub const MIN_PASSWORD: usize = 10;
/// Teto do tamanho aceito: o argon2 lê a senha inteira, e um corpo enorme seria
/// um jeito barato de gastar CPU.
pub const MAX_PASSWORD: usize = 256;

/// Hash argon2id (parâmetros padrão do crate: ~19 MiB, 2 passadas) em formato PHC.
pub fn hash(password: &str) -> Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("falha ao gerar o hash da senha: {e}"))
}

pub fn verify(hash: &str, password: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

/// Hash de uma senha que ninguém conhece. No login de uma conta que não existe
/// verificamos contra ele, para a resposta demorar o mesmo e não revelar quais
/// e-mails têm conta.
pub fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash("senha-que-ninguem-conhece").expect("argon2 com parâmetros padrão"))
}

/// Senha aleatória de 20 caracteres (~119 bits), para `hbt-server user add|reset`.
pub fn generate() -> String {
    rand::thread_rng()
        .sample_iter(Alphanumeric)
        .take(20)
        .map(char::from)
        .collect()
}

/// Validação deliberadamente simples: sem SMTP não há como confirmar o e-mail,
/// então só barramos o que claramente não é um (e o que quebraria a tela).
pub fn valid_email(email: &str) -> bool {
    let email = email.trim();
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    email.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrip_and_wrong_password() {
        let h = hash("uma senha boa").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify(&h, "uma senha boa"));
        assert!(!verify(&h, "outra senha"));
        assert!(!verify("lixo", "uma senha boa"), "hash inválido não verifica");
    }

    #[test]
    fn dummy_hash_never_matches() {
        assert!(!verify(dummy_hash(), ""));
        assert!(!verify(dummy_hash(), "senha"));
    }

    #[test]
    fn generated_passwords_are_long_and_different() {
        let (a, b) = (generate(), generate());
        assert_eq!(a.len(), 20);
        assert_ne!(a, b);
        assert!(a.len() >= MIN_PASSWORD);
    }

    #[test]
    fn email_validation() {
        for ok in ["a@b.co", "ana.silva+x@exemplo.com.br"] {
            assert!(valid_email(ok), "{ok}");
        }
        for bad in [
            "",
            "a",
            "a@b",
            "@b.co",
            "a@.co",
            "a@b.",
            "a b@c.co",
            "a@b@c.co",
            &format!("{}@x.co", "a".repeat(260)),
        ] {
            assert!(!valid_email(bad), "{bad}");
        }
    }
}
