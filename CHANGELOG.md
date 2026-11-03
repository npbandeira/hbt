# Changelog

Formato baseado em [Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/).
O workflow `release` publica como notas da release a seção da versão da tag.

## [Unreleased]

### Segurança

- **Teto de túneis simultâneos por token** (`--max-tunnels-per-token`, padrão 2). Com cadastro aberto, uma conta podia abrir túneis sem limite e esgotar a faixa de portas públicas. O padrão 2 deixa uma sessão antiga ainda não expirada (até ~10 s) conviver com a reconexão.

## [2.0.1] - 2026-10-29

Só a interface do site mudou: o protocolo e o banco são os mesmos da 2.0.0, então servidor e clientes 2.0.0 continuam compatíveis. Basta atualizar o `hbt-server`.

### Mudou (interface do site)

- **Site redesenhado** com a skill de design "tasteskill" (direção *One motif*): a porta numerada do hbt vira o disco magenta, que recorre como forma dos botões e da marca. Landing nova com um construtor de comando funcional (tipo de túnel e porta local geram o `hbt ...` e o endereço público), os três tipos de túnel, os quatro passos, os números medidos (perda de 0,05 % a 50 mil pacotes UDP por segundo, p99 de 3,7 ms, token conferido em 8,9 µs, com as condições do teste escritas) e o cadastro de verdade no fechamento.
- Painel com a lista de tokens ao lado do formulário de criar token e o passo-a-passo de instalar e configurar um computador; login e cadastro mostram o que você recebe ao criar um token.
- Fontes trocadas para League Spartan e Figtree, embutidas no binário (33 KB, antes 75 KB); a monoespaçada ficou só nos blocos de código, usando a do sistema.
- A landing não oferece cadastro quando o servidor roda com `--signup closed`.
- O tema escuro automático foi removido: a paleta "céu e areia" tem um tema só.
- O nome da marca na interface é **HBT**, em caixa alta (logotipo, título da aba e textos); comandos e binários seguem em minúsculas. O logotipo do rodapé é uma imagem com o nome "HBT" para leitores de tela.
- A skill de design `design-taste-frontend` passou a ser versionada em `.agents/` (com o link em `.claude/skills/`).

## [2.0.0] - 2026-10-29

Versão maior porque o transporte mudou e **servidor e clientes antigos não conversam com os novos**: atualize os dois lados juntos, libere `7000/udp` e configure o fingerprint em cada cliente (`hbt configure --fingerprint ...`; o servidor mostra com `hbt-server fingerprint`). Os tokens existentes continuam valendo.

### Adicionado

- **Site no próprio `hbt-server`** (`--web-bind`, atrás de um proxy com HTTPS): landing page, cadastro e login com e-mail e senha e um painel onde cada pessoa cria os próprios tokens e copia o comando `hbt configure ...` pronto. O cliente `hbt` não muda: não há login no CLI, ele só guarda o token.
- Cadastro aberto como o do ngrok, com cota de tokens por conta (`--max-tokens-per-user`, padrão 5), limite de tentativas por IP e `--signup closed` para fechar.
- `hbt-server user add|list|reset|disable|enable|remove`: `disable` bloqueia a conta e revoga todos os tokens dela (os túneis caem em ~10 s; a sessão do site cai na hora).
- O painel mostra se cada token está conectado agora.
- `scripts/build-css.sh` gera o CSS (Tailwind standalone); `install-server.sh` aceita `--web-bind`, `--web-url` e `--signup`.
- `hbt-server fingerprint`: mostra o fingerprint do certificado (e o gera se não existir).
- `hbt-server token create` imprime o comando `hbt configure` completo, já com o fingerprint.
- `hbt configure --fingerprint`, flag global `--fingerprint` e variável `HBT_FINGERPRINT` no cliente.
- Scripts de instalação aceitam `--fingerprint`; `install-server.sh` mostra o fingerprint e libera `7000/udp`.

### Mudou

As quatro primeiras mudanças do transporte **quebram a compatibilidade**: atualize servidor e clientes juntos.

- **Transporte trocado de TCP+yamux para QUIC** (`quinn`, TLS 1.3). Todo o tráfego de um túnel passa por uma conexão QUIC só: cada usuário TCP/HTTP é um stream, e a perda de um pacote não atrasa os outros.
- **A porta de controle (7000) agora é UDP.** Libere `7000/udp` no firewall; clientes atrás de um firewall que bloqueia UDP de saída não conectam (não há fallback para TCP).
- **UDP de verdade dentro do túnel**: cada datagrama público viaja como datagrama QUIC (RFC 9221), sem ordem nem retransmissão. Datagramas maiores que o MTU do caminho (~1200–1400 bytes) são descartados; em WireGuard use `MTU = 1280`.
- **O cliente confere a identidade do servidor por fingerprint.** O servidor gera um certificado autoassinado na primeira execução (`cert.der`/`key.der` em `--data-dir`) e o cliente só aceita o certificado cujo SHA-256 for o `--fingerprint` (`HBT_FINGERPRINT` ou o valor salvo por `hbt configure`).
- Cliente e servidor precisam estar na mesma versão. O protocolo antigo (V1) foi removido, e com ele o aviso "atualize o hbt".
- **Tokens passam a morar num banco SQLite** (`hbt.db` em `--data-dir`). O `tokens.json` existente é importado sozinho na primeira abertura (vira `tokens.json.migrated`) e os tokens continuam valendo. A consulta por token, que cada heartbeat faz, passou de 8,4 ms para 8,9 µs com 10 000 tokens, e as escritas do CLI e do site no mesmo banco não se perdem mais.
- `hbt-server token list` ganhou a coluna de dono.
- O build estático para musl agora exige um compilador C para musl (`sudo apt install musl-tools`); o `scripts/build.sh` avisa e usa um contorno se faltar.

### Corrigido

- `install-server.sh` chamava `sudo -u hbt` para mostrar o fingerprint mesmo no modo de teste `DESTDIR`.

### Removido

- Dependências `yamux`, `futures` e `tokio-util`.
- O limite de streams por semáforo e a calibração de janelas do yamux (agora são config do QUIC em `proto/src/quic.rs`).

### Desempenho

Teste de carga UDP (`scripts/loadtest.sh`, 10 peers, 512 B, servidor com 1 CPU), antes → depois:
| | TCP+yamux | QUIC |
|---|---|---|
| 10k pps, p99 | 2,96 ms | 0,62 ms |
| 50k pps, perda | 0,93% | 0,05% |
| 50k pps, p99 | 13,7 ms | 3,7 ms |
Medido no loopback do Docker, sem perda de rede induzida. Com 1000 peers a perda continua em ~99% (já acontecia antes; não investigado).

### Segurança

- O tráfego entre cliente e servidor agora é criptografado (TLS 1.3) e protegido contra homem-no-meio pelo fingerprint. O trecho público (usuário → porta pública) continua em claro.

## [0.1.1] - 2026-10-27

Versão de manutenção: sem mudanças de comportamento nem de protocolo.

### Mudou

- Código reorganizado em módulos: servidor (`cli`, `serve`, `control`, `tunnel`, `http_edge`, `state`) e cliente (`cli`, `session`, `forward`); testes e2e separados por área.
- `hbt-proto` expõe `ProtoError` (`thiserror`) em vez de `io::Error` genérico.

### Adicionado

- Documentação em todos os itens públicos do `hbt-proto` (`#![warn(missing_docs)]`).
- Lints no workspace (`unsafe` proibido, clippy) e mais testes unitários (`parse_range`, `validate_subdomain`, `bind_port`, roteamento por `Host`).
- Mapa do repositório no README.

## [0.1.0] - 2026-10-26

Primeira versão: túnel reverso (TCP, UDP e HTTP) em Rust, no estilo do ngrok.

### Adicionado

- Túneis **HTTP** por subdomínio (`hbt http 3000`), **TCP** (`hbt tcp 22`) e **UDP** (`hbt udp 51820`), com porta ou subdomínio escolhido ou sorteado.
- Uma conexão só por túnel: todo o tráfego multiplexado (yamux); uma conexão pública nova custa 1 RTT até o cliente.
- Tokens por cliente com limite de portas e subdomínios e revogação na hora (`hbt-server token create|list|revoke`).
- Métricas Prometheus (`/metrics`) e stack Prometheus + Grafana com dashboard (`scripts/observability.sh`).
- Reconexão automática com backoff exponencial; serviço systemd endurecido.
- `install.sh` com checksums e workflow de release (Linux x86_64/aarch64, macOS, Windows).

### Limitações conhecidas

- Sem TLS entre cliente e servidor (autenticação por desafio HMAC; tráfego em claro).
- UDP passava dentro de TCP: sob perda de pacotes a latência sobe.
- Até 1024 conexões simultâneas por túnel.

[Unreleased]: https://github.com/npbandeira/hbt/compare/v2.0.1...HEAD
[2.0.1]: https://github.com/npbandeira/hbt/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/npbandeira/hbt/compare/v0.1.1...v2.0.0
[0.1.1]: https://github.com/npbandeira/hbt/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/npbandeira/hbt/releases/tag/v0.1.0
