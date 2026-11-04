# hbt — reverse tunnel (TCP + UDP) em Rust

Expõe um serviço que está atrás de NAT/firewall através de uma porta pública
de um servidor na nuvem. Quem inicia todas as conexões é o **cliente**.

```
 usuário ──► [VPS: porta 2222] ◄══ túnel ══ [hbt cliente] ──► 127.0.0.1:22
```

## Instalação (release)

```sh
# cliente (Linux/macOS) em ~/.local/bin
curl -fsSL https://github.com/npbandeira/hbt/releases/latest/download/install.sh | sh
hbt configure --server SEU.VPS --token ID.SEGREDO --fingerprint FP

# servidor (Linux + systemd, no VPS)
curl -fsSL https://github.com/npbandeira/hbt/releases/latest/download/install.sh | sh -s -- --server --domain tunnel.exemplo.com
```

O `install.sh` detecta sistema e arquitetura, confere o SHA-256 e, se o
repositório for privado, baixa pelo `gh` autenticado. No Windows, baixe
`hbt-windows-x86_64.exe` da [página de releases](https://github.com/npbandeira/hbt/releases).
Para publicar uma versão: suba a versão nos `Cargo.toml`, mova o conteúdo de
`[Unreleased]` do `CHANGELOG.md` para uma seção `## [X.Y.Z] - AAAA-MM-DD` (ela vira
as notas da release) e envie a tag (`git tag vX.Y.Z && git push origin vX.Y.Z`);
o workflow `release` compila e publica.

## Instalação a partir do código (scripts)

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
`systemctl restart hbt-server`. Tokens e contas ficam em `/var/lib/hbt/hbt.db` (SQLite; só o usuário `hbt` lê).
O deploy exige ssh com `sudo` no VPS. Libere as portas no firewall ao final
(o script imprime os comandos; ele não mexe no firewall sozinho).

## Uso (estilo ngrok)

**1. No servidor (VPS com IP público):**

```sh
export HBT_DOMAIN=tunnel.exemplo.com           # domínio base dos túneis HTTP
hbt-server serve                               # controle :7000, HTTP :80, portas 20000-30000
```

Libere no firewall `7000/udp` (o controle é QUIC), `80/tcp` e `20000-30000` (tcp+udp), e crie o
DNS wildcard `*.tunnel.exemplo.com -> IP do servidor` (só para túneis HTTP).

**Tokens: um por cliente**, como no ngrok (o segredo é mostrado uma única vez):

```sh
hbt-server token create ana                          # sem restrições
hbt-server token create ana --subdomains ana,ana-dev --ports 20000-20010
hbt-server token list                                # sem segredos
hbt-server token revoke ana                          # vale na hora: derruba a sessão em ~10s
hbt-server fingerprint                               # identidade (certificado) do servidor
```

`--ports` e `--subdomains` limitam o que aquele token pode abrir. Os tokens
ficam em `--data-dir` (`HBT_DATA_DIR`, padrão `/var/lib/hbt`).

**Fingerprint:** na primeira execução o servidor gera um certificado
autoassinado em `--data-dir` (`cert.der`/`key.der`). O cliente só conecta se o
SHA-256 desse certificado for o `--fingerprint` configurado: é isso que impede
alguém de se passar pelo servidor, e o tráfego vai criptografado (TLS 1.3).
`hbt-server token create` já imprime o comando `hbt configure` completo. Apagar
`cert.der`/`key.der` gera outra identidade e exige reconfigurar os clientes.

**Entrar pelo site (cadastro e tokens, estilo ngrok):** com `--web-bind`, o servidor
serve uma landing page, o cadastro (e-mail e senha) e um painel onde cada pessoa
cria os próprios tokens e copia o comando `hbt configure ...` pronto. O `hbt`
(cliente) não faz login: só guarda o token.

```sh
hbt-server serve --web-bind 127.0.0.1:8081 --web-url https://meuhbt.duckdns.org
hbt-server user list                      # contas e quantos tokens cada uma tem
hbt-server user disable ana@exemplo.com   # bloqueia e revoga TODOS os tokens dela
```

- **O site precisa de HTTPS** (tem senha e token). Com `--https-bind` (veja **HTTPS**
  abaixo) ele aparece em `https://<domínio>` sem proxy nenhum. Sem isso, ponha
  Caddy/nginx com HTTPS na frente de `--web-bind`.
- `--signup closed` fecha o cadastro: só entram contas criadas por
  `hbt-server user add <e-mail>` (a senha é gerada e mostrada uma vez). O padrão,
  `open`, é como o ngrok; o abuso é limitado por `--max-tokens-per-user` (padrão 5),
  por limite de tentativas por IP e pelo `user disable`.
- **Subdomínios reservados:** no painel, cada conta reserva até 3 nomes
  (`--max-reserved-per-user`, `0` desliga) que só os tokens dela abrem. A reserva
  cai depois de 30 dias sem túnel usando o nome (`--reservation-days`) ou quando
  a conta é bloqueada ou apagada. Nomes como `www`, `api`, `admin` e `mail` não
  podem ser reservados nem abertos por contas do site (os tokens do admin podem).
- Sem verificação de e-mail (exigiria SMTP): quem se cadastra não prova que o e-mail é seu.
- As sessões ficam em memória: reiniciar o servidor desloga todos.
- O CSS do site é gerado pelo Tailwind e fica commitado. Se mudar classes em
  `server/src/web/`, rode `./scripts/build-css.sh` e commite o `server/assets/app.css`.
- Quem já usava o `tokens.json` não precisa fazer nada: na primeira abertura os tokens
  são importados para o banco (e o arquivo vira `tokens.json.migrated`).

**2. Na sua máquina, uma vez só:**

```sh
hbt configure --server tunnel.exemplo.com --token ID.SEGREDO --fingerprint FP
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

O cliente guarda servidor, token e fingerprint em `~/.config/hbt/config.toml`
(permissão 0600). Ordem de prioridade: flag (`--server`, `--token`,
`--fingerprint`) > variável (`HBT_SERVER`, `HBT_TOKEN`, `HBT_FINGERPRINT`) > arquivo. `hbt configure` sem argumentos mostra a
configuração atual. Para ver logs: `RUST_LOG=info hbt http 3000`.

**HTTPS**: com `--https-bind 0.0.0.0:443`, o próprio servidor termina o TLS.
Os túneis ficam em `https://<sub>.<domínio>`, o site (se `--web-bind` estiver
ligado) em `https://<domínio>`, e a porta 80 passa a redirecionar para https. O
certificado cobre `<domínio>` e `*.<domínio>` e fica em `<data-dir>/tls/`:

```sh
# Domínio grátis do DuckDNS (meuhbt.duckdns.org apontando para o IP do VPS):
# o servidor emite e renova o certificado sozinho no Let's Encrypt (DNS-01).
HBT_DUCKDNS_TOKEN=... hbt-server serve --domain meuhbt.duckdns.org \
  --https-bind 0.0.0.0:443 --web-bind 127.0.0.1:8081
```

O DuckDNS já resolve qualquer `*.meuhbt.duckdns.org` para o mesmo IP, então não
há DNS wildcard a criar. Em outro provedor de DNS, gere o certificado por fora
(p.ex. certbot) e copie `cert.pem` e `key.pem` para `<data-dir>/tls/`: o servidor
relê os arquivos de hora em hora. Para testar a configuração sem gastar a cota do
Let's Encrypt, use `--acme-staging` (o certificado sai, mas não é confiável).
Enquanto não há certificado, tudo segue em http.

**Testar tudo na própria máquina**, sem VPS nem DNS: `./scripts/demo-local.sh`
roda tudo automaticamente. Na mão:

```sh
export HBT_DATA_DIR=/tmp/hbt-dev HBT_DOMAIN=localhost HBT_HTTP_BIND=127.0.0.1:8080
hbt-server serve --control-bind 127.0.0.1:7000 &
TOKEN=$(hbt-server token create dev --quiet)
hbt configure --server 127.0.0.1 --token "$TOKEN" --fingerprint "$(hbt-server fingerprint)"
python3 -m http.server 3000 &
hbt http 3000 --subdomain demo          # abra http://demo.localhost:8080
```

Um processo `hbt` = um túnel; rode vários para expor várias portas. Cada túnel
usa **uma única conexão QUIC** (UDP) até o servidor: os usuários públicos de
TCP/HTTP passam multiplexados dentro dela (um stream por usuário, sem handshake
extra) e cada datagrama UDP viaja como *datagrama QUIC*, sem ordem nem
retransmissão, como o UDP original. Clientes atrás de um firewall que bloqueia
UDP de saída não conseguem conectar.

**MTU do UDP:** um datagrama UDP público maior que o MTU do caminho QUIC
(~1200–1400 bytes menos ~20–30 de cabeçalho do túnel) é descartado. Em
WireGuard, use `MTU = 1280`.

Cliente e servidor precisam falar a mesma versão do protocolo: ao atualizar o
`hbt-server`, atualize também os clientes (`hbt` antigo recebe "atualize o hbt").

## Estrutura do repositório

| Pasta | Conteúdo |
|---|---|
| `proto/` | protocolo (mensagens, framing), configuração QUIC/TLS (`quic.rs`) e multiplexação (`mux.rs`), compartilhados |
| `server/` | `hbt-server`: `cli.rs` (args, tokens, contas), `serve.rs` (listeners), `control.rs` (handshake/registro), `tunnel.rs` (sessão, TCP/UDP), `http_edge.rs` (roteamento por Host), `db.rs` (SQLite: tokens e contas), `accounts.rs` (senhas), `web/` (site: landing, cadastro, painel), `state.rs`, `tokens.rs`, `metrics.rs` |
| `client/` | `hbt`: `cli.rs` (args/credenciais), `session.rs` (conexão, reconexão, heartbeat), `forward.rs` (repasse TCP/UDP), `config.rs` |
| `e2e/` | testes ponta a ponta com os binários reais, por área: `tunnels`, `http`, `auth`, `configure`, `web` (helpers em `tests/common/`) |
| `bench/` | `hbt-udpbench`, usado pelo teste de carga |
| `scripts/` | build, deploy, instalação, demo e teste de carga |
| `docker/`, `observability/` | stack de teste de carga e Prometheus/Grafana |
| `docs/` | guia de arquitetura e notas de release |

## Desenvolvimento

Convenções: lints do workspace em `Cargo.toml` (`[workspace.lints]`), `unsafe`
proibido, sem `unwrap()` em caminhos de rede (só em locks e testes).

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Leia

[`docs/GUIDE.md`](docs/GUIDE.md): arquitetura, conceitos de Rust usados,
glossário e trade-offs das decisões.
