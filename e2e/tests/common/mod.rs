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

/// Uma porta livre (TCP e UDP) para o servidor de teste usar.
///
/// Não usa a porta 0 do SO: ela vem da faixa efêmera, a mesma das conexões de
/// saída, e com muitos testes em paralelo uma conexão podia pegar a porta antes do
/// servidor, que então morria com "endereço em uso" (falha intermitente no CI).
/// Aqui as portas vêm de 15000-29999, fora da faixa efêmera do Linux
/// (32768-60999) e do macOS (49152+), e um contador nunca repete uma porta dentro
/// do mesmo binário de teste (os binários rodam um de cada vez).
pub fn free_port() -> u16 {
    free_ports(1)
}

/// `n` portas livres seguidas (para faixas como `--port-range A-B`); devolve a primeira.
pub fn free_ports(n: u16) -> u16 {
    use std::sync::atomic::{AtomicU32, Ordering};
    const BASE: u32 = 15_000;
    const SPAN: u32 = 15_000;
    static NEXT: AtomicU32 = AtomicU32::new(u32::MAX);
    // Ponto de partida diferente a cada execução, para não repetir as portas da anterior.
    let _ = NEXT.compare_exchange(
        u32::MAX,
        std::process::id().wrapping_mul(7919) % SPAN,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    let n32 = u32::from(n);
    loop {
        let first = BASE + NEXT.fetch_add(n32, Ordering::SeqCst) % (SPAN - n32);
        let all_free = (first..first + n32).all(|p| {
            let p = p as u16;
            std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
                && std::net::UdpSocket::bind(("127.0.0.1", p)).is_ok()
        });
        if all_free {
            return first as u16;
        }
    }
}

/// Comando de um binário do projeto, com ambiente limpo de HBT_* herdados.
pub fn cmd(bin: &str) -> Command {
    let mut c = Command::new(bin_dir().join(bin));
    c.env_remove("HBT_TOKEN")
        .env_remove("HBT_SERVER")
        .env_remove("HBT_FINGERPRINT")
        .env_remove("HBT_DOMAIN")
        .env_remove("HBT_DATA_DIR")
        .env_remove("HBT_HTTPS_BIND")
        .env_remove("HBT_DUCKDNS_TOKEN")
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
        Fixture::start_prepared(range, extra, |_| {})
    }

    /// Como `start`, mas roda `prep(data_dir)` antes de subir o `serve`.
    pub fn start_prepared(range: &str, extra: &[&str], prep: impl FnOnce(&Path)) -> Fixture {
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
        prep(&dir);
        let mut c = cmd("hbt-server");
        c.args(["--data-dir"]).arg(&dir);
        c.args(["serve", "--control-bind", &format!("127.0.0.1:{control}")]);
        c.args(["--tunnel-ip", "127.0.0.1", "--port-range", range]);
        c.args(extra);
        Fixture {
            dir,
            control,
            fingerprint,
            _server: {
                // O log do servidor vai para um arquivo: se ele morrer na partida, o teste mostra por quê.
                let log = std::fs::File::create(std::env::temp_dir().join(format!("hbt-e2e-{control}.log")))
                    .unwrap();
                c.stdout(Stdio::null()).stderr(log).spawn().unwrap()
            },
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

// ------------------------------------------------------------- site (web)

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `nome=valor` do primeiro `Set-Cookie` (sem os atributos).
    pub fn cookie(&self) -> Option<String> {
        self.header("set-cookie")
            .and_then(|c| c.split(';').next())
            .filter(|c| !c.ends_with('='))
            .map(str::to_owned)
    }
}

/// Codifica um valor de formulário (`application/x-www-form-urlencoded`).
pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Uma requisição HTTP/1.1 crua, `Connection: close`, e a resposta inteira.
pub async fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    timeout(Duration::from_secs(10), s.read_to_end(&mut raw))
        .await
        .expect("o site não respondeu")
        .unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("resposta sem cabeçalhos");
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    Resp {
        status,
        headers,
        body: body.to_owned(),
    }
}

/// Um navegador de mentira: guarda o cookie e manda `Origin` do próprio site.
pub struct Browser {
    pub port: u16,
    pub cookie: Option<String>,
}

impl Browser {
    pub fn new(port: u16) -> Browser {
        Browser { port, cookie: None }
    }

    pub async fn send(&mut self, method: &str, path: &str, body: &str, htmx: bool) -> Resp {
        let origin = format!("http://127.0.0.1:{}", self.port);
        let mut h = vec![("Origin", origin.as_str())];
        if htmx {
            h.push(("HX-Request", "true"));
        }
        let cookie = self.cookie.clone();
        if let Some(c) = &cookie {
            h.push(("Cookie", c));
        }
        let r = http(self.port, method, path, &h, body).await;
        if let Some(c) = r.cookie() {
            self.cookie = Some(c);
        }
        r
    }

    pub async fn signup(&mut self, email: &str, password: &str) -> Resp {
        self.send(
            "POST",
            "/signup",
            &form(&[("email", email), ("password", password)]),
            false,
        )
        .await
    }

    pub async fn login(&mut self, email: &str, password: &str) -> Resp {
        self.send(
            "POST",
            "/login",
            &form(&[("email", email), ("password", password)]),
            false,
        )
        .await
    }

    pub async fn create_token(&mut self, name: &str) -> Resp {
        self.send("POST", "/tokens", &form(&[("name", name)]), true).await
    }
}

/// O `hbt configure ...` que o painel mostra: (server, token, fingerprint).
pub fn parse_configure(html: &str) -> (String, String, String) {
    let start = html
        .find("hbt configure")
        .expect("comando de configuração na resposta");
    let cmd = html[start..].split("</code>").next().unwrap();
    let word_after = |flag: &str| {
        cmd.split_once(flag)
            .unwrap_or_else(|| panic!("falta {flag} em: {cmd}"))
            .1
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    };
    (
        word_after("--server "),
        word_after("--token "),
        word_after("--fingerprint "),
    )
}

impl Fixture {
    /// Servidor com o site ligado; devolve (fixture, porta do site).
    pub async fn with_web(range: &str, extra: &[&str]) -> (Fixture, u16) {
        let web = free_port();
        let web_bind = format!("127.0.0.1:{web}");
        let mut args = vec!["--web-bind", web_bind.as_str()];
        args.extend_from_slice(extra);
        let fx = Fixture::start(range, &args);
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", web)).await.is_ok() {
                return (fx, web);
            }
            sleep(Duration::from_millis(100)).await;
        }
        let log = std::fs::read_to_string(std::env::temp_dir().join(format!("hbt-e2e-{}.log", fx.control)))
            .unwrap_or_default();
        panic!("o site não subiu em 10 s. Log do servidor:\n{log}");
    }

    /// `hbt-server <args...>` (sem o prefixo `token`), como o admin faria.
    pub fn admin(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(bin_dir().join("hbt-server"))
            .arg("--data-dir")
            .arg(&self.dir)
            .args(args)
            .output()
            .unwrap()
    }
}
