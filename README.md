# hbt — reverse tunnel (TCP + UDP) em Rust

Expõe um serviço que está atrás de NAT/firewall através de uma porta pública
de um servidor na nuvem. Quem inicia todas as conexões é o **cliente**.

```
 usuário ──► [VPS: porta 2222] ◄══ túnel ══ [hbt cliente] ──► 127.0.0.1:22
```

## Uso (estilo ngrok)

**1. No servidor (VPS com IP público):**

```sh
export HBT_TOKEN=$(openssl rand -hex 32)      # guarde este valor
export HBT_DOMAIN=tunnel.exemplo.com           # domínio base dos túneis HTTP
hbt-server                                     # controle :7000, HTTP :80, portas 20000-30000
```

Libere no firewall `7000/tcp`, `80/tcp` e `20000-30000` (tcp+udp), e crie o
DNS wildcard `*.tunnel.exemplo.com -> IP do servidor` (só para túneis HTTP).

**2. Na sua máquina, uma vez só:**

```sh
hbt configure --server tunnel.exemplo.com --token SEU_TOKEN
```

**3. Sempre que quiser expor algo:**

```sh
hbt http 3000                    # http://a1b2c3d4.tunnel.exemplo.com -> 127.0.0.1:3000
hbt http 3000 --subdomain app    # http://app.tunnel.exemplo.com
hbt tcp 22                       # tcp://tunnel.exemplo.com:2xxxx  -> ssh local
hbt udp 51820                    # udp://tunnel.exemplo.com:2xxxx  -> wireguard local
hbt tcp 22 --remote-port 22022   # escolhe a porta pública
hbt http 192.168.0.10:8080       # expõe outra máquina da sua rede local
```

O cliente guarda servidor e token em `~/.config/hbt/config.toml` (permissão
0600). Ordem de prioridade: flag (`--server`, `--token`) > variável
(`HBT_SERVER`, `HBT_TOKEN`) > arquivo. `hbt configure` sem argumentos mostra a
configuração atual. Para ver logs: `RUST_LOG=info hbt http 3000`.

**HTTPS**: o servidor fala HTTP puro; ponha Caddy/nginx com certificado
wildcard na frente (`HBT_HTTP_BIND=127.0.0.1:8080`) e repasse para ele.

**Testar tudo na própria máquina**, sem VPS nem DNS:

```sh
HBT_TOKEN=x HBT_DOMAIN=localhost HBT_HTTP_BIND=127.0.0.1:8080 hbt-server &
hbt configure --server 127.0.0.1 --token x
python3 -m http.server 3000 &
hbt http 3000 --subdomain demo          # abra http://demo.localhost:8080
```

Um processo `hbt` = um túnel; rode vários para expor várias portas.

## Desenvolvimento

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Leia

[`docs/GUIDE.md`](docs/GUIDE.md): arquitetura, conceitos de Rust usados,
glossário e trade-offs das decisões.
