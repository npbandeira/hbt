//! Utilitários compartilhados pelos testes e2e: fixtures, servidores de eco e helpers.
#![allow(dead_code)] // cada arquivo de teste usa só parte destes helpers

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

/// Compila os binários uma vez e devolve o diretório onde ficam.
pub fn bin_dir() -> PathBuf {
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = std::process::Command::new(cargo)
            .args(["build", "-q", "-p", "hbt-server", "-p", "hbt"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "falha ao compilar os binários");
    });
    // .../target/debug/deps/tunnel-xxxx  =>  .../target/debug
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p
}

/// Pede uma porta livre ao SO (bind na porta 0) e a devolve.
/// Há uma pequena janela de corrida até o processo usá-la; aceitável em testes.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Comando de um binário do projeto, com ambiente limpo de HBT_* herdados.
pub fn cmd(bin: &str) -> Command {
    let mut c = Command::new(bin_dir().join(bin));
    c.env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER")
        .env_remove("HBT_FINGERPRINT")
        .env_remove("HBT_DOMAIN")
        .env_remove("HBT_DATA_DIR")
        .kill_on_drop(true); // sem isso, falha de teste deixaria processos órfãos
    c
}

pub fn spawn_quiet(mut c: Command) -> Child {
    c.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
}

/// Servidor de teste: diretório de dados próprio + processo `serve`.
pub struct Fixture {
    pub dir: PathBuf,
    pub control: u16,
    /// Fingerprint do certificado do servidor, que o cliente precisa conferir.
    pub fingerprint: String,
    pub _server: Child,
}

impl Fixture {
    /// `range` = faixa pública de portas; `extra` = flags extras do `serve`.
    pub fn start(range: &str, extra: &[&str]) -> Fixture {
        let control = free_port();
        let dir = std::env::temp_dir().join(format!("hbt-e2e-{}-{control}", std::process::id()));
        // Gera o certificado antes do `serve`, para já termos o fingerprint.
        let out = std::process::Command::new(bin_dir().join("hbt-server"))
            .arg("--data-dir")
            .arg(&dir)
            .arg("fingerprint")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let fingerprint = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        let mut c = cmd("hbt-server");
        c.args(["--data-dir"]).arg(&dir);
        c.args(["serve", "--control-bind", &format!("127.0.0.1:{control}")]);
        c.args(["--tunnel-ip", "127.0.0.1", "--port-range", range]);
        c.args(extra);
        Fixture {
            dir,
            control,
            fingerprint,
            _server: spawn_quiet(c),
        }
    }

    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.control)
    }

    /// `hbt-server token <args...>` e devolve o stdout.
    pub fn token_cmd(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(bin_dir().join("hbt-server"))
            .arg("--data-dir")
            .arg(&self.dir)
            .arg("token")
            .args(args)
            .output()
            .unwrap()
    }

    /// Cria um token e devolve a string `<id>.<segredo>`.
    pub fn token(&self, name: &str, extra: &[&str]) -> String {
        let mut args = vec!["create", name, "--quiet"];
        args.extend_from_slice(extra);
        let out = self.token_cmd(&args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    pub fn client(&self, token: &str, args: &[&str]) -> Command {
        let mut c = cmd("hbt");
        c.args(["--server", &self.addr(), "--fingerprint", &self.fingerprint])
            .args(args)
            .env("HBT_TOKEN", token);
        c
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub async fn tcp_echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

pub async fn udp_echo_server() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = s.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = s.recv_from(&mut buf).await {
            let _ = s.send_to(&buf[..n], from).await;
        }
    });
    port
}

/// Tenta por até ~10s fazer um eco "ping" pelo túnel TCP.
pub async fn tcp_echo_works(port: u16) -> bool {
    for _ in 0..50 {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await {
            if s.write_all(b"ping").await.is_ok() {
                let mut buf = [0u8; 4];
                if let Ok(Ok(_)) = timeout(Duration::from_millis(500), s.read_exact(&mut buf)).await {
                    return &buf == b"ping";
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Sobe o cliente com stdout capturado e devolve a URL do banner "Forwarding".
pub async fn banner_url(mut c: Command) -> (Child, String) {
    let mut child = c.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let url = timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Some(rest) = line.strip_prefix("Forwarding") {
                return rest.split_whitespace().next().unwrap().to_owned();
            }
        }
        panic!("cliente terminou sem imprimir o banner");
    })
    .await
    .expect("sem banner em 10s");
    (child, url)
}

/// Servidor HTTP mínimo: responde "ok" a qualquer requisição.
pub async fn http_ok_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .await;
            });
        }
    });
    port
}

/// Faz um GET cru para o edge HTTP com o `Host` dado e devolve a resposta inteira
/// (vazia se o servidor ainda está subindo).
pub async fn http_get(edge: u16, host: &str) -> String {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", edge)).await else {
        return String::new();
    };
    let req = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    let _ = timeout(Duration::from_secs(5), s.read_to_string(&mut out)).await;
    out
}

/// Repete o GET até a resposta conter `needle` (ou ~10s).
pub async fn http_wait(edge: u16, host: &str, needle: &str) -> String {
    let mut resp = String::new();
    for _ in 0..50 {
        resp = http_get(edge, host).await;
        if resp.contains(needle) {
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    resp
}

/// Servidor com HTTP em `localhost`; devolve (fixture, porta do edge).
pub fn http_fixture() -> (Fixture, u16) {
    let edge = free_port();
    let public = free_port();
    let fx = Fixture::start(
        &format!("{public}-{public}"),
        &[
            "--domain",
            "localhost",
            "--http-bind",
            &format!("127.0.0.1:{edge}"),
        ],
    );
    (fx, edge)
}

pub fn isolated_config(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hbt-e2e-{tag}-{}", std::process::id()))
}

pub fn hbt_output(home: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(bin_dir().join("hbt"))
        .args(args)
        .env("XDG_CONFIG_HOME", home)
        .env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER")
        .env_remove("HBT_FINGERPRINT")
        .output()
        .unwrap()
}
