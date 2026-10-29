# Guia: como o hbt funciona e o que você aprende lendo o código

## 1. Arquitetura

Workspace Cargo com 5 crates:

| crate | tipo | papel |
|---|---|---|
| `proto` | lib | mensagens, framing, autenticação, multiplexação (compartilhado) |
| `server` | bin `hbt-server` | roda na nuvem |
| `client` | bin `hbt` | CLI |
| `e2e` | testes | sobe os binários reais e passa tráfego |
| `bench` | bin `hbt-udpbench` | gerador de carga UDP do teste de carga |

**Workspace**: vários crates num repositório compartilhando `Cargo.lock` e
`target/`. Evita compilar dependências duas vezes e permite que cliente e
servidor usem o *mesmo* crate de protocolo.

### Uma conexão QUIC por túnel, iniciada pelo cliente

1. **Conexão e handshake**: o cliente abre uma conexão QUIC (UDP) na porta de
   controle; o TLS 1.3 só aceita o certificado cujo SHA-256 é o `--fingerprint`
   configurado (`proto/src/quic.rs`). No primeiro stream bidirecional ele manda
   `Hello`, responde ao desafio HMAC e registra o túnel (`Register` →
   `Registered`). Esse stream segue como **stream de controle**: heartbeats nos
   dois sentidos.
2. **Tráfego** (`proto/src/mux.rs`), tudo dentro da mesma conexão:
   - **TCP/HTTP**: o servidor abre um *stream* por usuário público; começa com
     um byte de tipo (o QUIC só avisa o par de um stream novo quando chega o
     primeiro byte) e depois são bytes crus;
   - **UDP**: cada datagrama público vira um *datagrama QUIC* (RFC 9221) com um
     `UdpFrame`, sem ordem nem retransmissão.

```
usuário ──► [porta pública] ──► servidor abre stream nº N ══ conexão QUIC (UDP) ══► cliente ──► serviço local
```

Abrir um stream não tem handshake: o servidor já manda os dados junto com o
cabeçalho do stream. Por isso uma conexão pública nova custa 1 RTT até o
cliente (a requisição em si), e não ~3 RTT como na primeira versão, em que o
cliente abria uma conexão TCP nova até o servidor para cada usuário.

### Túneis HTTP (`hbt http 3000`)

Em vez de uma porta pública por túnel, o servidor tem UMA porta de entrada
HTTP. Para cada conexão ele lê os cabeçalhos, extrai `Host: abc.dominio.com`,
procura `abc` na tabela de rotas (`subdomínio -> sessão do cliente`) e a partir
daí reaproveita exatamente o mecanismo TCP (um stream novo na sessão), só
reenviando antes os bytes já lidos. Se não houver rota: 404; sessão caída:
502; túnel no limite de conexões simultâneas por 10s: 504.

## 2. Glossário

- **Virtual host / `Host` header**: vários sites no mesmo IP:porta; o
  cliente diz em `Host:` qual quer. É assim que `a.x.com` e `b.x.com` chegam
  a túneis diferentes. Exige DNS wildcard (`*.x.com`).
- **Edge**: a "borda" pública que recebe o tráfego da internet e o encaminha.
- **Reverse tunnel**: o serviço fica "atrás"; o cliente faz conexão de *saída*
  até o servidor público, que então encaminha tráfego de volta por ela.
  NAT e firewalls costumam bloquear entrada, não saída.
- **NAT**: roteador que compartilha um IP público entre muitos dispositivos;
  por isso eles não são alcançáveis de fora sem ajuda.
- **Async / `.await`**: em vez de uma thread por conexão, milhares de tarefas
  leves cedem a vez enquanto esperam I/O. `async fn` devolve um *future* (uma
  máquina de estados que só avança quando alguém a "poll"a).
- **Tokio**: o *runtime* que executa futures: agenda tasks em um pool de threads
  e fala com epoll/io_uring do SO. Rust não traz runtime async na std.
- **Task (`tokio::spawn`)**: "thread verde" gerenciada pelo Tokio. Exige
  `'static` + `Send`: não pode emprestar variáveis locais; por isso clonamos
  `Arc`s e `String`s antes do `spawn`.
- **Framing / length-prefix**: TCP é fluxo de bytes sem fronteiras. Prefixamos
  cada mensagem com seu tamanho (u32) para saber onde termina.
- **Serialização (serde + bincode)**: transforma structs/enums em bytes.
- **HMAC / challenge-response / nonce / replay**: o segredo nunca trafega. O
  servidor manda um número aleatório (nonce), o cliente devolve
  `HMAC-SHA256(segredo, nonce)`. Gravar a resposta não adianta: o nonce muda.
- **Timing attack**: comparar segredos byte a byte e parar no primeiro erro
  vaza informação pelo tempo. `verify_slice` compara em tempo constante.
- **Heartbeat**: mensagem periódica para detectar par morto (cabo puxado, NAT
  que esqueceu a conexão), pois nesses casos o TCP não avisa por minutos.
- **Exponential backoff**: reconexão com espera crescente (1s, 2s, 4s… 30s).
- **Multiplexação / stream**: várias conversas independentes dentro de uma
  conexão. Cada pedaço de dado leva o número do stream a que pertence; o outro
  lado separa por esse número. Cada stream tem controle de fluxo próprio
  (janela), então um usuário lento não enche a memória nem trava os outros
  por falta de espaço.
- **Cancel safety**: um future descartado no meio (ex.: perder um `select!`)
  pode ter consumido bytes. `read_exact` não é cancel safe; `mpsc::recv` e
  esperar `JoinHandle` são.
- **Backpressure**: canais `mpsc` com capacidade (64/256). Se o consumidor
  é lento, o produtor espera em vez de encher a memória.
- **HOL blocking (head-of-line)**: num único stream ordenado, um pacote
  atrasado atrasa todos os que vêm depois.

## 3. Conceitos de Rust, onde ver no código

| Conceito | Onde | Explicação |
|---|---|---|
| Ownership/borrow | `write_msg(&mut stream, &msg)` | `&` empresta somente leitura, `&mut` empresta para escrita exclusiva; o dono segue sendo quem criou. |
| `Result` e `?` | por todo lado | Erros são valores. `?` propaga o erro para quem chamou (como `throw` explícito no tipo). |
| `anyhow` vs tipos próprios | `Fatal` no cliente | `anyhow::Error` é um "erro qualquer" com contexto (bom em apps). Para decidir por tipo (`downcast_ref`) criamos `Fatal`. Em bibliotecas prefira enums de erro (`thiserror`). |
| `enum` com dados + `match` | `ServerMsg`, `Hello` | O compilador força tratar todas as variantes: adicionar uma mensagem nova quebra a compilação onde falta tratá-la (bom!). |
| `let ... else` | `negotiate` | Desestrutura ou sai: `let ClientMsg::Auth(m) = x else { bail!() }`. |
| Genéricos + traits | `read_msg<R: AsyncRead + Unpin, T>` | Mesma função para TcpStream, buffers em memória (testes)... resolvido na compilação. |
| `Arc<T>` | `State`, sockets UDP | Dono compartilhado com contagem atômica. `Rc` seria a versão single-thread (não é `Send`). |
| `Mutex` | `routes`, `sessions` | `std::sync::Mutex` para seções curtas sem `.await`; `tokio::sync::Mutex` só se precisar segurar o lock através de um await. |
| RAII / `Drop` | `AbortOnDrop` | Limpeza automática ao sair de escopo, em qualquer caminho, inclusive erro. |
| `mpsc`, `oneshot` | `proto/src/mux.rs` | Passar mensagens entre tasks em vez de compartilhar memória. Quem quer um stream manda um `oneshot` ao task que gira a sessão e espera a resposta por ele (entrega única). |
| `poll_fn` / `Poll` | `mux::run` | Um future escrito à mão: a cada "poll" atendemos pedidos de stream e fazemos a sessão andar; `Poll::Pending` devolve a vez ao runtime, que nos acorda quando houver I/O. |
| `select!` | `run_tunnel`, `main` | Espera vários futures; o primeiro que completar vence e os outros são descartados. |
| Lifetimes | quase invisíveis aqui | Evitamos ao clonar/`Arc` antes de `spawn`; o compilador infere o resto. |
| `#[cfg(test)]` | `proto` | Código de teste só compilado em `cargo test`. |

## 4. Trade-offs das decisões

**Multiplexação (QUIC) vs. uma conexão por usuário**
A primeira versão abria uma conexão TCP nova do cliente até o servidor para
cada usuário público. Medido com ping de 63 ms até o VPS: uma requisição com
conexão nova levava ~200 ms (~3 RTT); com keep-alive, ~70 ms. Sob ~60
conexões novas por segundo, a rede do cliente (NAT/roteador) passou a perder
pacotes e apareceram 504s. Com muitas conexões curtas, o cliente também
esgotava as portas efêmeras (TIME_WAIT). Depois tudo passou por uma conexão TCP
só com yamux, como frp/rathole, mas um pacote perdido atrasava todos os
usuários do túnel até a retransmissão (HOL blocking) e o UDP herdava isso.
Hoje a conexão é QUIC (`quinn`): os streams são independentes (a perda num não
atrasa os outros) e o UDP usa datagramas não confiáveis.

**Limite de streams por conexão**
`max_concurrent_bidi_streams` (1024, em `quic.rs`) é o teto de conexões
simultâneas por túnel: acima disso, `open_bi` espera vaga (até 10s, depois 504)
em vez de derrubar o túnel. Janelas de stream e de envio são folgadas (8 e 16
MiB) para não limitar a vazão em links de alta latência.

**Sem compatibilidade com o transporte antigo**
A troca TCP+yamux → QUIC não é compatível: cliente e servidor precisam estar na
mesma versão, e a porta de controle passou de TCP para UDP (libere `7000/udp`).
Não há fallback para TCP: clientes atrás de firewall que bloqueia UDP de saída
não conectam.

**Identidade do servidor: certificado autoassinado + fingerprint**
Não há CA nem domínio: o servidor gera o certificado na primeira execução
(`cert.der`/`key.der` em `--data-dir`) e o cliente só aceita o certificado com o
SHA-256 configurado (`hbt configure --fingerprint`). É *pinning*: quem não
conhece o fingerprint não consegue se passar pelo servidor. Custo: trocar o
certificado exige reconfigurar os clientes.

**UDP em datagramas QUIC**
Cada datagrama público vira um datagrama QUIC com um `UdpFrame` (peer + dados):
sem ordem, sem retransmissão, sem head-of-line blocking. Custo: o datagrama
precisa caber no MTU do caminho (~1200–1400 bytes menos ~20–30 de cabeçalho do
`UdpFrame`); maiores são descartados, como um roteador faria (WireGuard: use
`MTU = 1280`). Melhoria futura: framing binário compacto (`[família][ip][porta]
[dados]`) no lugar de bincode.

**Sessões UDP e timeout de 60s**
UDP não tem conexão; "sessão" = peer público. Cada peer ganha um socket local
(origens distintas para o serviço). Ocioso 60s mede só respostas do serviço
local: se um peer só envia e o serviço nunca responde, a sessão é recriada
(porta de origem local muda). Melhoria futura: contar atividade nos dois sentidos.

**Tokens por cliente (`<id>.<segredo>`)**
O `id` (público) localiza o registro; o `segredo` (256 bits) prova identidade
via desafio HMAC. O servidor guarda `SHA-256("hbt-token-v1:"+segredo)` e nunca o
segredo cru; o cliente recalcula a mesma chave. Cada token tem nome, data,
dono (a conta do site que o criou, se houver) e limites opcionais
(portas/subdomínios), e pode ser revogado sozinho: o servidor consulta o banco a
cada autenticação e a cada heartbeat, então a revogação derruba sessões ativas
em até ~10s. Um id desconhecido recebe o desafio como qualquer
outro e falha com a mesma mensagem (evita descobrir quais ids existem).
*Limite honesto:* quem copia o `hbt.db` tem as chaves e consegue responder
ao desafio ("pass-the-hash"): o servidor ainda guarda a chave derivada, não um
hash dela. Agora que o canal é TLS, essa migração é possível; o formato já está
pronto.

**SQLite (e não JSON) para tokens e contas**
O servidor consulta o token a cada handshake e a cada heartbeat (10 s) de cada
sessão, e dois processos escrevem (o servidor, pelo site, e o `hbt-server
token|user ...` do admin). Com um `tokens.json` cada consulta relia e reparseava o
arquivo inteiro: medido com 10 000 tokens (2,3 MB), 8,4 ms por consulta contra 8,9
µs no SQLite (`cargo test -p hbt-server --release -- --ignored --nocapture
heartbeat_lookup`). Com milhares de sessões isso seria uma CPU inteira de um VPS de
1 vCPU, no worker do Tokio (parando o relay, inclusive o UDP). Além disso, duas
escritas simultâneas no JSON perdiam uma; no SQLite (WAL + `busy_timeout`) o banco
serializa entre processos. `rusqlite` (embutido, `bundled`) e não `sqlx`: o `sqlx`
usa o mesmo SQLite em C por baixo, seu async é um wrapper sobre threads e as macros
`query!` pedem banco ou metadados no build. Custo: o build precisa de compilador C
(o `ring` do QUIC já exigia) e, para o alvo musl, de `musl-gcc`.

**O site: contas, cotas e defesas**
HTML no servidor (`maud`, que escapa tudo), trechos trocados pelo htmx e CSS do
Tailwind commitado. Cadastro aberto como o do ngrok, então o abuso é contido por:
cota de tokens por conta (`--max-tokens-per-user`), limite de tentativas de login
(10 por 15 min) e de cadastro (5 por hora) por IP, `hbt-server user disable`
(bloqueia e revoga todos os tokens numa transação) e `--signup closed`. A senha é
argon2id; um login de conta inexistente verifica contra um hash falso (mesma
demora, mesma mensagem), e só 2 hashes rodam ao mesmo tempo (cada um usa ~19 MiB).
A sessão é um id aleatório em memória (cookie `HttpOnly; SameSite=Strict`, `Secure`
atrás de HTTPS); a conta é relida do banco a cada requisição, então `user disable`
vale na hora mesmo sendo outro processo. CSRF: além do `SameSite`, o `Origin` precisa
ser o próprio site e as ações do painel exigem o cabeçalho `HX-Request`. O CSP só
aceita arquivos do próprio site (sem script nem estilo inline). Não há verificação
de e-mail (exigiria SMTP) e as sessões não sobrevivem a reinício.

**Autenticação HMAC dentro de TLS**
O HMAC prova que o cliente conhece o segredo e impede replay; o canal QUIC/TLS
1.3 (com o fingerprint conferido) criptografa o tráfego entre cliente e
servidor e impede o homem-no-meio. O trecho público (usuário → porta pública)
continua em claro, como qualquer porta TCP/UDP: se o serviço exposto não usa
TLS/SSH, o payload não é protegido nesse lado.

**bincode vs JSON**
bincode é compacto e rápido, mas ilegível e sem versionamento. JSON é
depurável a olho, porém maior e mais lento; para datagramas UDP em array de
bytes seria bem pior. Em protocolo público considere um campo de versão.

**Rotear HTTP por conexão (L4) vs. por requisição (L7)**
Lemos só a primeira requisição para escolher o túnel e depois copiamos bytes
(camada 4). Vantagem: WebSocket, SSE, uploads gigantes e HTTP/1.1 keep-alive
funcionam sem tratamento especial, e não há parsing de corpo (superfície de
ataque menor). Custo: não adicionamos `X-Forwarded-For`, não temos inspeção/
replay de requisições como o painel do ngrok, e uma conexão que misture
`Host`s diferentes seria roteada só pelo primeiro. Um proxy L7 completo
(hyper) resolve isso a custo de bem mais código.

**HTTPS**
O edge fala HTTP puro. Terminar TLS aqui exigiria certificados wildcard
(ACME DNS-01) e `rustls`: ótimo exercício futuro. Hoje a recomendação é
deixar Caddy/nginx com o certificado na frente: separa responsabilidades e
usa software maduro para renovar certificados.

**Subdomínios**
Sorteados (32 bits aleatórios) ou pedidos pelo cliente, validados como label DNS.
Com token compartilhado, qualquer cliente pode pegar qualquer subdomínio livre
(quem chega primeiro leva) e a checagem+inserção acontece sob o mesmo lock para
evitar condição de corrida.

**Um túnel por processo `hbt`**
Simples e isolado (uma falha não derruba as outras). Custo: uma conexão por
túnel. Evolução: arquivo de config com vários túneis na mesma sessão (o
protocolo de streams já comporta isso).

**Faixa de portas + token único**
Todos os clientes compartilham o mesmo token e podem pedir qualquer porta da
faixa (primeiro que chegar ganha). Para multiusuário: token por cliente e
portas atribuídas a cada um. Também: um cliente autenticado pode fazer o
servidor enviar UDP para destinos arbitrários (abuso de reflexão); aceitável
com clientes confiáveis.

**Limites ainda ausentes (por escolha)**
Sem rate limit de tentativas de autenticação. Em produção adicione-o (`tower`,
fail2ban lendo o log). Conexões simultâneas por túnel são limitadas (1024) e
há métricas Prometheus (`--metrics-bind`).

## 5. Boas práticas aplicadas

- `cargo fmt` e `cargo clippy -D warnings` limpos (CI roda os dois).
- Limite no tamanho de frame (evita DoS por alocação).
- Timeouts em todo handshake (evita conexões penduradas / slowloris).
- Comparação de segredo em tempo constante; token por env var, não por argumento.
- Nada de `unwrap()` em caminhos de rede; só em locks (um `unwrap` em
  `Mutex::lock` só falha se outro thread entrou em pânico segurando-o) e testes.
- Tasks auxiliares presos a guards RAII: nada vaza quando a sessão cai.
- Testes unitários + e2e com os binários reais.

## 6. Para estudar na ordem

1. `proto/src/lib.rs` (tipos, genéricos, framing)
2. `proto/src/mux.rs` (multiplexação, um future escrito à mão com `poll_fn`)
3. `client/src/` (`cli.rs` clap → `session.rs` retry/heartbeat → `forward.rs` repasse TCP/UDP)
4. `server/src/` (`state.rs` estado compartilhado → `control.rs` handshake → `tunnel.rs` select/RAII → `http_edge.rs`)
5. `e2e/tests/` (`common/` fixtures; `tunnels.rs`, `http.rs`, `auth.rs`, `configure.rs`: como testar sistemas de rede)

Recursos: *The Rust Book* (doc.rust-lang.org/book), *Tokio tutorial*
(tokio.rs/tokio/tutorial), *Rust by Example*.
