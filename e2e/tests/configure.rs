//! `hbt configure` e configuração do cliente.

mod common;

use common::*;

#[tokio::test]
async fn configure_saves_credentials_so_no_flags_are_needed() {
    let public = free_port();
    let fx = Fixture::start(&format!("{public}-{public}"), &[]);
    let token = fx.token("t", &[]);
    let echo = tcp_echo_server().await;

    // Diretório de config isolado: o teste nunca toca no ~/.config real.
    let home = fx.dir.join("home");
    let hbt = bin_dir().join("hbt");
    let ok = std::process::Command::new(&hbt)
        .args(["configure", "--server", &fx.addr(), "--token", &token])
        .args(["--fingerprint", &fx.fingerprint])
        .env("XDG_CONFIG_HOME", &home)
        .env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER")
        .status()
        .unwrap()
        .success();
    assert!(ok);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("hbt/config.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "arquivo com segredo deve ser privado");
    }

    // Agora SEM --server, SEM --token e SEM env: tudo vem do arquivo.
    let mut c = cmd("hbt");
    c.args(["tcp", &echo.to_string(), "--remote-port", &public.to_string()])
        .env("XDG_CONFIG_HOME", &home);
    let (_client, url) = banner_url(c).await;
    assert_eq!(url, format!("tcp://127.0.0.1:{public}"));
}

#[test]
fn client_without_configuration_explains_what_to_do() {
    let home = isolated_config("empty");
    let out = hbt_output(&home, &["http", "3000"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("hbt configure"));
}

#[test]
fn configure_rejects_malformed_tokens() {
    let home = isolated_config("badtoken");
    let out = hbt_output(&home, &["configure", "--server", "x", "--token", "sem-formato"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("<id>.<segredo>"));
    assert!(
        !home.join("hbt/config.toml").exists(),
        "não deve gravar token inválido"
    );
}
