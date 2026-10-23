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

# Na sua máquina:
export HBT_TOKEN='um-segredo-longo-e-aleatorio'
hbt --server meu.vps:7000 tcp --local 127.0.0.1:22 --remote-port 22022
hbt --server meu.vps:7000 udp --local 127.0.0.1:51820 --remote-port 25182
```

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
