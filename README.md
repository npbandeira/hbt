# hbt — reverse tunnel (TCP + UDP) em Rust

Expõe um serviço que está atrás de NAT/firewall através de uma porta pública
de um servidor na nuvem. Quem inicia todas as conexões é o **cliente**.

```
 usuário ──► [VPS: porta 2222] ◄══ túnel ══ [hbt cliente] ──► 127.0.0.1:22
```

## Uso

```sh
cargo build --release          # binários em target/release/{hbt-server,hbt}

# Na nuvem (libere 7000/tcp e a faixa de portas públicas no firewall):
export HBT_TOKEN='um-segredo-longo-e-aleatorio'   # ex: openssl rand -hex 32
hbt-server --control-bind 0.0.0.0:7000 --port-range 20000-30000

# Para túneis HTTP (estilo ngrok), informe o domínio base e a porta de entrada:
hbt-server --domain tunnel.exemplo.com --http-bind 0.0.0.0:80

# Na sua máquina:
export HBT_TOKEN='um-segredo-longo-e-aleatorio'
hbt --server meu.vps:7000 http 3000                      # => http://a1b2c3d4.tunnel.exemplo.com
hbt --server meu.vps:7000 http 3000 --subdomain meuapp   # => http://meuapp.tunnel.exemplo.com
hbt --server meu.vps:7000 tcp --local 127.0.0.1:22 --remote-port 22022
hbt --server meu.vps:7000 udp --local 127.0.0.1:51820 --remote-port 25182
```

**DNS para HTTP**: crie um registro wildcard `*.tunnel.exemplo.com -> IP do servidor`.
**HTTPS**: o servidor fala HTTP puro; ponha Caddy/nginx com certificado
wildcard na frente (`--http-bind 127.0.0.1:8080`) e eles repassam para o hbt.
Teste local sem DNS: `curl -H 'Host: meuapp.tunnel.exemplo.com' http://meu.vps/`.

Um processo `hbt` = um túnel; rode vários para expor várias portas.
Logs: `RUST_LOG=debug hbt ...`.

## Desenvolvimento

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Leia

[`docs/GUIDE.md`](docs/GUIDE.md): arquitetura, conceitos de Rust usados,
glossário e trade-offs das decisões.
