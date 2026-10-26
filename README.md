# hbt — reverse tunnel (TCP + UDP) em Rust

Expõe um serviço que está atrás de NAT/firewall através de uma porta pública
de um servidor na nuvem. Quem inicia todas as conexões é o **cliente**.

```
 usuário ──► [VPS: porta 2222] ◄══ túnel ══ [hbt cliente] ──► 127.0.0.1:22
```

## Instalação rápida (scripts)

```sh
./scripts/build.sh                                    # testes + binários em dist/
./scripts/deploy.sh usuario@meu.vps --domain tunnel.exemplo.com   # VPS + cliente local, tudo de uma vez
```

| Script | Onde roda | O que faz |
|---|---|---|
| `scripts/demo-local.sh` | sua máquina | **demonstração completa local**: servidor + site + túneis HTTP/TCP + limites + revogação, tudo em diretório temporário (`--keep` deixa no ar) |
| `scripts/loadtest.sh` | sua máquina (Docker) | **teste de carga**: servidor em container com 1 CPU e 2 GB; mede túneis HTTP, TCP (oha) e UDP (`hbt-udpbench`). `MODES=udp` roda só um tipo |
| `scripts/observability.sh` | sua máquina (Docker) | **observabilidade**: Prometheus + Grafana locais com dashboard pronto; `up vps` abre um túnel SSH até as métricas do servidor (`127.0.0.1:9477` no VPS) |
| `scripts/build.sh` | sua máquina | roda os testes e gera `dist/` (nativo + estático musl) |
| `scripts/install-server.sh` | **no VPS** | instala o serviço systemd `hbt-server` e mostra as portas a liberar. Idempotente: atualiza mantendo os tokens |
| `scripts/install-client.sh` | sua máquina | copia `hbt` para `~/.local/bin` e (com `--server/--token`) já configura |
| `scripts/deploy.sh` | sua máquina | build + envia por ssh + instala no VPS + cria um token exclusivo para esta máquina e configura o cliente |

Depois do deploy, no VPS: `journalctl -u hbt-server -f` (logs) e
`systemctl restart hbt-server`. Os tokens ficam em `/var/lib/hbt/tokens.json` (só o usuário `hbt` lê).
O deploy exige ssh com `sudo` no VPS. Libere as portas no firewall ao final
(o script imprime os comandos; ele não mexe no firewall sozinho).

## Uso (estilo ngrok)

**1. No servidor (VPS com IP público):**

```sh
export HBT_DOMAIN=tunnel.exemplo.com           # domínio base dos túneis HTTP
hbt-server serve                               # controle :7000, HTTP :80, portas 20000-30000
```

Libere no firewall `7000/tcp`, `80/tcp` e `20000-30000` (tcp+udp), e crie o
DNS wildcard `*.tunnel.exemplo.com -> IP do servidor` (só para túneis HTTP).

**Tokens: um por cliente**, como no ngrok (o segredo é mostrado uma única vez):

```sh
hbt-server token create ana                          # sem restrições
hbt-server token create ana --subdomains ana,ana-dev --ports 20000-20010
hbt-server token list                                # sem segredos
hbt-server token revoke ana                          # vale na hora: derruba a sessão em ~10s
```

`--ports` e `--subdomains` limitam o que aquele token pode abrir. Os tokens
ficam em `--data-dir` (`HBT_DATA_DIR`, padrão `/var/lib/hbt`).

**2. Na sua máquina, uma vez só:**

```sh
hbt configure --server tunnel.exemplo.com --token ID.SEGREDO
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

**Testar tudo na própria máquina**, sem VPS nem DNS: `./scripts/demo-local.sh`
roda tudo automaticamente. Na mão:

```sh
export HBT_DATA_DIR=/tmp/hbt-dev HBT_DOMAIN=localhost HBT_HTTP_BIND=127.0.0.1:8080
hbt-server serve --control-bind 127.0.0.1:7000 &
TOKEN=$(hbt-server token create dev --quiet)
hbt configure --server 127.0.0.1 --token "$TOKEN"
python3 -m http.server 3000 &
hbt http 3000 --subdomain demo          # abra http://demo.localhost:8080
```

Um processo `hbt` = um túnel; rode vários para expor várias portas. Cada túnel
usa **uma única conexão TCP** até o servidor: todos os usuários públicos passam
multiplexados dentro dela (yamux), sem handshake extra por conexão.

Cliente e servidor precisam falar a mesma versão do protocolo: ao atualizar o
`hbt-server`, atualize também os clientes (`hbt` antigo recebe "atualize o hbt").

## Desenvolvimento

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Leia

[`docs/GUIDE.md`](docs/GUIDE.md): arquitetura, conceitos de Rust usados,
glossário e trade-offs das decisões.
