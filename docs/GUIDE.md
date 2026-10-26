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

### Uma conexão TCP por túnel, iniciada pelo cliente

1. **Handshake**: o cliente conecta na porta de controle, manda `Hello`,
   responde ao desafio HMAC e registra o túnel (`Register` → `Registered`).
2. **Sessão multiplexada**: a partir daí a MESMA conexão vira uma sessão
   [yamux](https://github.com/hashicorp/yamux/blob/master/spec.md)
   (`proto/src/mux.rs`). O servidor abre *streams* lógicos dentro dela; cada
   stream começa com um byte de tipo:
   - **controle** (um por sessão): heartbeats nos dois sentidos;
   - **conexão** (um por usuário público, TCP ou HTTP): bytes crus;
   - **UDP** (um por túnel): todos os datagramas em `UdpFrame`s.

```
usuário ──► [porta pública] ──► servidor abre stream nº N ══ sessão (1 conexão TCP) ══► cliente ──► serviço local
```

Abrir um stream não tem handshake: o servidor já manda os dados junto com o
cabeçalho do stream. Por isso uma conexão pública nova custa 1 RTT até o
cliente (a requisição em si), e não ~3 RTT como no protocolo antigo, em que o
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

**Multiplexação (yamux) vs. uma conexão por usuário**
A primeira versão abria uma conexão TCP nova do cliente até o servidor para
cada usuário público. Medido com ping de 63 ms até o VPS: uma requisição com
conexão nova levava ~200 ms (~3 RTT); com keep-alive, ~70 ms. Sob ~60
conexões novas por segundo, a rede do cliente (NAT/roteador) passou a perder
pacotes e apareceram 504s. Com muitas conexões curtas, o cliente também
esgotava as portas efêmeras (TIME_WAIT). Hoje tudo passa por uma conexão só,
como frp/rathole: nada disso existe. Custo: um pacote perdido nessa conexão
atrasa todos os usuários do túnel até a retransmissão (HOL blocking), e todo o
tráfego passa por um task só por sessão. Num servidor com 1 CPU isso não
reduziu a vazão de forma relevante depois de bufferizar o socket (o yamux
escreve cabeçalho e corpo de cada frame em chamadas separadas).
Alternativa sem HOL blocking: QUIC (`quinn`), que multiplexa sobre UDP.

**Limite de streams por sessão**
O yamux derruba a sessão INTEIRA se o número de streams passar do limite
configurado, e só "esquece" um stream fechado um pouco depois. Por isso o
limite dele é 2048 e o nosso, controlado por um semáforo, é 1024 conexões
simultâneas por túnel: acima disso, conexões novas esperam vaga (até 10s,
depois 504) em vez de derrubar o túnel.

**Compatibilidade entre versões**
O bincode identifica variantes de enum pela posição, então as mensagens do
protocolo antigo continuam declaradas (`V1Control`, `V1NewConn`...) só para
não deslocar as outras. Um cliente antigo recebe "atualize o hbt" e para,
em vez de tentar reconectar para sempre; um cliente novo num servidor antigo
avisa que o servidor parece antigo.

**UDP dentro de TCP**
Nossa escolha: datagramas viajam num stream TCP com framing. Simples e
atravessa qualquer firewall. Custo: TCP garante ordem e retransmite, o que
anula o ponto do UDP (latência baixa tolerando perda): sob perda de pacotes
você tem HOL blocking e picos de latência. Ruim para jogos/voz sob rede ruim;
OK para DNS, WireGuard, syslog. Alternativa melhor: QUIC datagrams (crate
`quinn`), ou UDP direto cliente↔servidor com hole punching (mais complexo).

**Sessões UDP e timeout de 60s**
UDP não tem conexão; "sessão" = peer público. Cada peer ganha um socket local
(origens distintas para o serviço). Ocioso 60s mede só respostas do serviço
local: se um peer só envia e o serviço nunca responde, a sessão é recriada
(porta de origem local muda). Melhoria futura: contar atividade nos dois sentidos.

**Tokens por cliente (`<id>.<segredo>`)**
O `id` (público) localiza o registro; o `segredo` (256 bits) prova identidade
via desafio HMAC. O servidor guarda `SHA-256("hbt-token-v1:"+segredo)` e nunca o
segredo cru; o cliente recalcula a mesma chave. Cada token tem nome, data e
limites opcionais (portas/subdomínios), e pode ser revogado sozinho: o servidor
relê o arquivo a cada autenticação e a cada heartbeat, então a revogação derruba
sessões ativas em até ~10s. Um id desconhecido recebe o desafio como qualquer
outro e falha com a mesma mensagem (evita descobrir quais ids existem).
*Limite honesto:* sem TLS, quem copia `tokens.json` tem as chaves e consegue
responder ao desafio ("pass-the-hash"): guardar a chave derivada só protege de
verdade quando o canal for TLS e o servidor guardar um hash dela. O formato
já está pronto para essa migração. Armazenamento em JSON atende dezenas de
clientes; para muitos milhares, use SQLite.

**Autenticação HMAC sem criptografia**
Prova que o cliente conhece o segredo e impede replay, mas o tráfego
trafega em claro entre cliente e servidor, e um atacante no meio da rede
poderia sequestrar a conexão depois do handshake. Para a internet pública,
envolva em TLS (`tokio-rustls`) ou rode o túnel dentro de WireGuard/SSH. Se o
serviço exposto já usa TLS/SSH (HTTPS, ssh), o payload já é protegido.
Escolhemos HMAC primeiro para manter o foco no assunto "túnel"; TLS é o
próximo passo natural.

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
3. `client/src/main.rs` (clap, async, retry)
4. `server/src/main.rs` (estado compartilhado, canais, select)
5. `e2e/tests/tunnel.rs` (como testar sistemas de rede)

Recursos: *The Rust Book* (doc.rust-lang.org/book), *Tokio tutorial*
(tokio.rs/tokio/tutorial), *Rust by Example*.
