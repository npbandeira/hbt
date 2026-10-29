//! HTML das páginas. As classes são do Tailwind (tema e componentes em
//! `assets/tailwind.css`). Direção visual: "One motif" (tasteskill): a porta
//! numerada do hbt vira o disco magenta, que recorre como forma dos botões e da marca.

use maud::{html, Markup, DOCTYPE};

use super::assets;
use crate::tokens::{format_date, TokenRecord};

// Mensagens: dizem o que aconteceu e o que fazer (iguais para conta inexistente
// e senha errada, para não revelar quais e-mails têm conta).
pub const BAD_LOGIN: &str = "E-mail ou senha incorretos.";
pub const TOO_MANY: &str = "Muitas tentativas. Aguarde alguns minutos e tente de novo.";
pub const BLOCKED: &str = "Esta conta está bloqueada. Fale com o administrador do servidor.";
pub const SIGNUP_CLOSED: &str = "Este servidor não aceita novos cadastros. Peça uma conta ao administrador.";
pub const BAD_EMAIL: &str = "Informe um e-mail válido, como nome@exemplo.com.";
pub const SHORT_PASSWORD: &str = "A senha precisa ter pelo menos 10 caracteres.";
pub const LONG_PASSWORD: &str = "A senha pode ter no máximo 256 caracteres.";
pub const EMAIL_TAKEN: &str = "Já existe uma conta com este e-mail. Entre ou use outro.";
pub const NAME_TAKEN: &str = "Você já tem um token com este nome. Escolha outro.";
pub const NAME_REQUIRED: &str = "Dê um nome ao dispositivo para criar o token.";
pub const BAD_NAME: &str = "Use um nome de até 64 caracteres, sem caracteres de controle.";

pub fn quota(max: usize) -> String {
    format!("Você já tem {max} tokens. Revogue um para criar outro.")
}

/// O htmx só troca o conteúdo de respostas 2xx/3xx por padrão; os erros de
/// formulário voltam como 422 e precisam ser trocados também.
const HTMX_CONFIG: &str = r#"{"includeIndicatorStyles":false,"allowEval":false,"allowScriptTags":false,"responseHandling":[{"code":"204","swap":false},{"code":"[23]..","swap":true},{"code":"422","swap":true},{"code":"[45]..","swap":false,"error":true}]}"#;

const INSTALL: &str = "curl -fsSL https://github.com/npbandeira/hbt/releases/latest/download/install.sh | sh";

#[derive(Clone, Copy)]
enum Nav<'a> {
    Visitor { signup_open: bool },
    User(&'a str),
}

fn shell(title: &str, nav: Nav<'_>, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="pt-BR" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="htmx-config" content=(HTMX_CONFIG);
                title { (title) " · HBT" }
                link rel="stylesheet" href=(assets::url("app.css"));
                // Sem defer de propósito: marca html.js antes da primeira pintura (as
                // entradas só escondem o conteúdo quando há JavaScript para mostrá-lo).
                script src=(assets::url("app.js")) {}
                script src=(assets::url("htmx.min.js")) defer {}
            }
            body class="min-h-screen" {
                header class="frame flex items-center justify-between gap-4 py-5" {
                    a href="/" class="flex items-center gap-2 font-display text-lead font-bold tracking-tighter" aria-label="HBT, página inicial" {
                        span class="disc size-4" aria-hidden="true" {}
                        "HBT"
                    }
                    nav class="flex items-center gap-3 sm:gap-6" aria-label="Principal" {
                        @match nav {
                            Nav::Visitor { signup_open } => {
                                a href="#instalar" class="hidden underline-offset-4 hover:underline sm:inline" { "Instalar" }
                                a href="/login" class="underline-offset-4 hover:underline" { "Entrar" }
                                @if signup_open { a href="/signup" class="btn" { "Criar conta" } }
                            }
                            Nav::User(email) => {
                                span class="hidden text-muted sm:inline" { (email) }
                                a href="/dashboard" class="underline-offset-4 hover:underline" { "Painel" }
                                form method="post" action="/logout" {
                                    button type="submit" class="cursor-pointer underline underline-offset-4" { "Sair" }
                                }
                            }
                        }
                    }
                }
                (body)
            }
        }
    }
}

// ---------------------------------------------------------------- landing

/// Um fragmento de interface marcado `data-ui` (o inspetor trata como imagem).
fn terminal(id: &str, command: &str, url: &str, local: &str) -> Markup {
    html! {
        div id=(id) data-ui class="ui-dark min-h-56 p-6 sm:p-8" role="tabpanel" {
            pre class="whitespace-pre-wrap [overflow-wrap:anywhere] leading-relaxed" {
                code { span class="text-sun" { "$ " } (command) "\n\n"
                    "Forwarding    " (url) "  ->  " (local) }
            }
        }
    }
}

pub fn landing(signup_open: bool) -> Markup {
    let cta = if signup_open {
        ("Criar conta", "/signup")
    } else {
        ("Entrar", "/login")
    };
    shell(
        "Túnel reverso",
        Nav::Visitor { signup_open },
        &html! {
            main {
            // ------------------------------------------------------------ herói
            section class="relative isolate overflow-hidden" {
                div class="disc disc-settle absolute -z-10 aspect-square w-[min(120vh,110vw)] right-[-30vw] top-[38%] lg:right-[-28vw] lg:top-[-12vh]" aria-hidden="true" {}
                div class="frame grid min-h-[min(calc(100svh-5rem),44rem)] items-center gap-12 pb-10 pt-4 lg:grid-cols-[1fr_minmax(0,34rem)] lg:pb-12" {
                    div {
                        h1 class="rise max-w-[14ch] text-display" { "Seu localhost, na internet." }
                        p class="rise d1 mt-6 max-w-[40ch] text-lead" {
                            "Crie uma conta, gere um token e cole um comando. O HBT abre a porta no servidor e leva o tráfego até o seu computador."
                        }
                        div class="rise d2 mt-8 flex flex-wrap gap-3" {
                            a href=(cta.1) class="btn" { (cta.0) }
                            a href="#instalar" class="btn btn-soft" { "Ver instalação" }
                        }
                    }
                    div id="builder" data-ui class="shutter ui-dark p-6 sm:p-8" {
                        h2 class="font-sans text-lead font-semibold tracking-normal leading-snug" { "Qual serviço roda na sua máquina?" }
                        div class="seg mt-4" role="tablist" aria-label="Tipo de túnel" {
                            button type="button" role="tab" aria-selected="true" data-kind="http" { "Site (http)" }
                            button type="button" role="tab" aria-selected="false" data-kind="tcp" { "TCP" }
                            button type="button" role="tab" aria-selected="false" data-kind="udp" { "UDP" }
                        }
                        label class="mt-5 block" {
                            span class="mb-1 block font-semibold" { "Porta local" }
                            input id="builder-port" class="field max-w-40" type="text" inputmode="numeric" pattern="[0-9]*" maxlength="5" value="3000" autocomplete="off";
                        }
                        p class="mt-6 font-semibold" { "Rode na sua máquina" }
                        pre class="code mt-2" { code id="builder-cmd" { "hbt http 3000" } }
                        p class="mt-5 font-semibold" { "O endereço público" }
                        pre class="code mt-2" { code id="builder-addr" { "http://app.seu-servidor" } }
                        button type="button" class="btn btn-soft mt-5 !border-sky !text-sky hover:!bg-sky hover:!text-ink" data-copy="#builder-cmd" { "Copiar comando" }
                    }
                }
            }

            // ------------------------------------------------------- 3 tipos
            section id="tipos" class="overflow-hidden bg-sand py-20 lg:py-24" {
                div class="frame grid items-center gap-12 lg:grid-cols-[1fr_1.1fr]" {
                    div {
                        h2 class="reveal max-w-[12ch] text-head" { "Três tipos de túnel." }
                        p class="reveal d1 mt-6 max-w-[40ch] text-lead" {
                            "Site, SSH, WireGuard, banco de dados: o que roda na sua máquina fica acessível de fora."
                        }
                        div class="reveal d2 seg mt-8" role="tablist" aria-label="Escolha um túnel" id="kinds" {
                            button type="button" role="tab" aria-selected="true" data-show="show-http" { "http" }
                            button type="button" role="tab" aria-selected="false" data-show="show-tcp" { "tcp" }
                            button type="button" role="tab" aria-selected="false" data-show="show-udp" { "udp" }
                        }
                        ul class="reveal d3 mt-6 max-w-[44ch] space-y-3" id="kind-facts" {
                            li data-fact="show-http" { strong { "http: " } "um endereço web só seu, escolhido pelo servidor ou por você com " code { "--subdomain" } "." }
                            li data-fact="show-tcp" { strong { "tcp: " } "uma porta pública dedicada, para SSH, bancos e qualquer serviço TCP." }
                            li data-fact="show-udp" { strong { "udp: " } "UDP de verdade, sem fila: cada pacote vai e volta como datagrama, ótimo para WireGuard e jogos." }
                        }
                    }
                    div class="reveal d2 relative grid place-items-center" {
                        div class="disc aspect-square w-[min(56vh,86vw)] !bg-sun" aria-hidden="true" {}
                        div class="absolute inset-0 m-auto h-fit w-[min(34rem,92%)]" id="kind-panels" {
                            (terminal("show-http", "hbt http 3000", "http://a1b2c3d4.seu-servidor", "127.0.0.1:3000"))
                            (terminal("show-tcp", "hbt tcp 22", "tcp://seu-servidor:24817", "127.0.0.1:22"))
                            (terminal("show-udp", "hbt udp 51820", "udp://seu-servidor:24817", "127.0.0.1:51820"))
                        }
                    }
                }
            }

            // -------------------------------------------------------- passos + instalar
            section class="py-20 lg:py-24" {
                div class="frame" {
                    h2 class="reveal max-w-[14ch] text-head" { "Quatro passos até o túnel." }
                    ol data-ui class="reveal d1 mt-12 flex min-h-64 flex-col gap-1.5 lg:flex-row" {
                        (stage("bg-accent text-accent-ink", "Crie a conta", &html! { "Com e-mail e senha, neste site." }))
                        (stage("bg-ink text-sky", "Crie um token", &html! { "No painel, dê um nome ao dispositivo." }))
                        (stage("bg-sun text-ink", "Cole o comando", &html! { "O painel mostra o " code { "hbt configure" } " pronto." }))
                        (stage("bg-sand text-ink", "Abra o túnel", &html! { code { "hbt http 3000" } }))
                    }
                    div id="instalar" class="mt-24 grid scroll-mt-8 gap-10 lg:grid-cols-[1fr_1.1fr] lg:items-start" {
                        div {
                            h3 class="reveal max-w-[14ch] text-head" { "Instale o HBT." }
                            p class="reveal d1 mt-4 max-w-[44ch]" {
                                "Linux e macOS. No Windows, baixe o " code { "hbt-windows-x86_64.exe" } " na página de releases do projeto."
                            }
                            p class="reveal d2 mt-4 max-w-[44ch]" {
                                "A conexão usa QUIC com TLS 1.3 e só aceita o certificado cujo fingerprint está no comando de configuração. O token nunca trafega: ele só assina um desafio."
                            }
                        }
                        div data-ui class="reveal d2 ui-dark p-5 sm:p-7" {
                            p class="font-semibold" { "1. Instale" }
                            pre class="code mt-2" { code id="install-cmd" { (INSTALL) } }
                            p class="mt-5 font-semibold" { "2. Cole o comando que o painel mostra" }
                            pre class="code mt-2" { code { "hbt configure --server SEU_SERVIDOR --token SEU_TOKEN --fingerprint FINGERPRINT" } }
                            button type="button" class="btn mt-5 !bg-sky !text-ink" data-copy="#install-cmd" { "Copiar a instalação" }
                        }
                    }
                }
            }

            // ------------------------------------------------------------- medido
            section class="bg-surface py-20 lg:py-24" {
                div class="frame" {
                    h2 class="reveal max-w-[12ch] text-head" { "Medido, não prometido." }
                    div class="mt-12 grid gap-1.5 lg:grid-cols-12 lg:grid-rows-[auto_auto]" {
                        div class="reveal d1 flex min-h-[56vw] flex-col justify-between rounded-2xl bg-accent p-6 text-accent-ink sm:p-8 lg:col-span-6 lg:row-span-2 lg:min-h-[70vh]" {
                            p class="font-display text-display font-bold leading-none tracking-tighter" { "0,05%" }
                            p class="max-w-[28ch] text-lead" { "de perda a 50 mil pacotes UDP por segundo." }
                        }
                        div class="reveal d2 flex min-h-48 flex-col justify-between rounded-2xl bg-ink p-6 text-sky lg:col-span-6 lg:col-start-7 lg:mt-12" {
                            p class="font-display text-head font-bold leading-none tracking-tighter" { "3,7 ms" }
                            p class="max-w-[30ch] text-lead" { "de latência no p99, nesse mesmo teste." }
                        }
                        div class="reveal d3 flex min-h-48 flex-col justify-between rounded-2xl bg-sun p-6 lg:col-span-4 lg:col-start-9" {
                            p class="font-display text-head font-bold leading-none tracking-tighter" { "8,9 µs" }
                            p class="max-w-[24ch] text-lead" { "para conferir um token entre 10 000." }
                        }
                    }
                    p class="reveal mt-8 max-w-[56ch] text-sm" {
                        "Teste de carga com o servidor limitado a 1 CPU e 2 GB, 10 origens e pacotes de 512 bytes, no loopback do Docker, sem perda de rede induzida. O roteiro e os números completos estão no guia do projeto."
                    }
                }
            }

            // -------------------------------------------------------------- fechamento
            section class="overflow-hidden bg-accent py-20 text-accent-ink lg:py-0" {
                div class="frame grid min-h-[72svh] items-center gap-12 lg:grid-cols-[1fr_minmax(0,28rem)]" {
                    div {
                        h2 class="reveal max-w-[12ch] text-display" { "Crie o primeiro token." }
                        p class="reveal d1 mt-6 max-w-[38ch] text-lead" {
                            "O token aparece uma única vez, junto com o comando pronto para colar no terminal."
                        }
                    }
                    div data-ui class="reveal d2 ui p-6 text-ink sm:p-8" {
                        @if signup_open {
                            h3 class="font-sans text-lead font-semibold tracking-normal leading-tight" { "Criar conta" }
                            (auth_fields("/signup", "Criar conta", "new-password", "", None, &html! { span class="text-sm text-muted" { "Pelo menos 10 caracteres." } }))
                            p class="mt-5" { "Já tem conta? " a href="/login" class="font-semibold underline underline-offset-4" { "Entrar" } }
                        } @else {
                            h3 class="font-sans text-lead font-semibold tracking-normal leading-tight" { "Cadastro fechado" }
                            p class="mt-3" { (SIGNUP_CLOSED) }
                            a href="/login" class="btn mt-5" { "Entrar" }
                        }
                    }
                }
            }
            }

            // ------------------------------------------------------------ rodapé
            footer class="overflow-hidden bg-ink pt-16 text-sky" {
                div class="frame" {
                    div class="flex flex-wrap items-start justify-between gap-8" {
                        p class="max-w-[30ch] text-lead" { "Túnel reverso para TCP, UDP e HTTP." }
                        nav class="flex flex-wrap gap-x-8 gap-y-3" aria-label="Rodapé" {
                            a href="/login" class="underline underline-offset-4" { "Entrar" }
                            @if signup_open { a href="/signup" class="underline underline-offset-4" { "Criar conta" } }
                            a href="#instalar" class="underline underline-offset-4" { "Instalar" }
                            a href="https://github.com/npbandeira/hbt/releases" class="underline underline-offset-4" { "Releases" }
                        }
                    }
                    div class="mark mt-12" role="img" aria-label="HBT" {
                        span class="bg-sun" { "H" }
                        span class="bg-sky" { "B" }
                        span class="bg-accent !text-accent-ink" { "T" }
                    }
                }
                p class="frame pb-6 pt-4 text-sm" { "hbt-server " (env!("CARGO_PKG_VERSION")) }
            }
        },
    )
}

fn stage(color: &str, title: &str, clause: &Markup) -> Markup {
    html! {
        li class=(format!("flex flex-1 flex-col justify-between gap-6 rounded-2xl p-6 min-h-36 lg:min-h-56 {color}")) {
            h3 class="text-lead" { (title) }
            p  { (clause) }
        }
    }
}

// ------------------------------------------------------------ cadastro/login

/// Os campos e o botão de um formulário de conta (usado nas telas e na landing).
fn auth_fields(
    action: &str,
    submit: &str,
    autocomplete: &str,
    email: &str,
    error: Option<&str>,
    hint: &Markup,
) -> Markup {
    html! {
        @if let Some(msg) = error {
            p class="notice mt-5" role="alert" { (msg) }
        }
        form method="post" action=(action) hx-boost="true" class="mt-5 grid gap-5" {
            label class="grid gap-1" {
                span class="font-semibold" { "E-mail" }
                input class="field" type="email" name="email" value=(email) required autocomplete="email";
            }
            label class="grid gap-1" {
                span class="font-semibold" { "Senha" }
                input class="field" type="password" name="password" required autocomplete=(autocomplete) maxlength="256";
                (hint)
            }
            button type="submit" class="btn" { (submit) }
        }
    }
}

/// A tela de conta: formulário à esquerda e, à direita, um campo de areia com o
/// disco do motivo (o campo fecha o vão do outro lado da grade; some no celular).
fn auth_page(title: &str, body: &Markup) -> Markup {
    html! {
        main class="grid min-h-[calc(100svh-5rem)] lg:grid-cols-2" {
            div class="flex items-center px-[var(--margin)] pb-16 pt-6" {
                div class="w-full max-w-md" {
                    h1 class="text-lead" { (title) }
                    (body)
                }
            }
            div class="relative hidden place-items-center overflow-hidden bg-sand lg:grid" {
                div class="disc aspect-square w-[min(62%,62vh)]" aria-hidden="true" {}
                div data-ui class="ui-dark absolute inset-0 m-auto h-fit w-[min(30rem,88%)] p-6 sm:p-8" {
                    p class="font-semibold" { "O que você recebe ao criar um token" }
                    pre class="code mt-3" { code { "hbt configure --server SEU_SERVIDOR --token a1b2c3d4.… --fingerprint 7236ee…" } }
                    p class="mt-4 text-sm" { "Exemplo. O token de verdade aparece uma única vez, no painel." }
                }
            }
        }
    }
}

pub fn login(error: Option<&str>, email: &str) -> Markup {
    shell(
        "Entrar",
        Nav::Visitor { signup_open: true },
        &auth_page(
            "Entrar",
            &html! {
                (auth_fields("/login", "Entrar", "current-password", email, error, &html! {}))
                p class="mt-6" { "Ainda não tem conta? " a href="/signup" class="font-semibold underline underline-offset-4" { "Criar conta" } }
            },
        ),
    )
}

pub fn signup(open: bool, error: Option<&str>, email: &str) -> Markup {
    let body = if open {
        html! {
            (auth_fields("/signup", "Criar conta", "new-password", email, error, &html! { span class="text-sm text-muted" { "Pelo menos 10 caracteres." } }))
            p class="mt-6" { "Já tem conta? " a href="/login" class="font-semibold underline underline-offset-4" { "Entrar" } }
        }
    } else {
        html! {
            p class="notice mt-5" role="alert" { (SIGNUP_CLOSED) }
            p class="mt-6" { "Já tem conta? " a href="/login" class="font-semibold underline underline-offset-4" { "Entrar" } }
        }
    };
    shell(
        "Criar conta",
        Nav::Visitor { signup_open: open },
        &auth_page("Criar conta", &body),
    )
}

// ---------------------------------------------------------------- painel

pub fn dashboard(email: &str, tokens: &[TokenRecord], online: &[bool]) -> Markup {
    let connected = online.iter().filter(|o| **o).count();
    shell(
        "Painel",
        Nav::User(email),
        &html! {
            main class="frame pb-24 pt-6" {
                h1 class="text-lead" { "Seus tokens" }
                p class="mt-2 max-w-[60ch]" {
                    @if tokens.is_empty() {
                        "Cada token autentica um computador. O comando de configuração aparece só quando você cria o token."
                    } @else {
                        (connected) " de " (tokens.len()) " conectados agora. O comando de configuração aparece só quando você cria o token."
                    }
                }

                div class="mt-8 grid gap-x-10 gap-y-12 lg:grid-cols-[minmax(0,1fr)_26rem] lg:items-start" {
                    div class="lg:col-start-1 lg:row-start-1" {
                        ul id="token-rows" {
                            @for (t, on) in tokens.iter().zip(online) { (token_row(t, *on)) }
                        }
                        @if tokens.is_empty() {
                            p id="empty" class="rounded-2xl bg-surface px-5 py-6" { "Você ainda não tem tokens. Crie o primeiro ao lado." }
                        }
                    }
                    aside class="rounded-2xl bg-surface p-6 lg:col-start-2 lg:row-span-2 lg:row-start-1" {
                        h2 class="font-sans text-lead font-semibold tracking-normal leading-tight" { "Criar token" }
                        (create_form(false))
                        div id="new-token" class="mt-6" aria-live="polite" {}
                    }
                    section class="rounded-2xl bg-ink p-6 text-sky sm:p-8 lg:col-start-1 lg:row-start-2" {
                        h2 class="font-sans text-lead font-semibold tracking-normal leading-tight" { "Configurar um computador" }
                        p class="mt-4 font-semibold" { "1. Instale o HBT" }
                        pre class="code mt-2" { code { (INSTALL) } }
                        p class="mt-4 font-semibold" { "2. Cole o comando do token" }
                        p class="mt-1" { "Ele aparece ao criar o token. Depois é só abrir o túnel, por exemplo " code { "hbt http 3000" } "." }
                    }
                }
            }
        },
    )
}

fn create_form(oob: bool) -> Markup {
    html! {
        form id="create-form" hx-post="/tokens" hx-target="#new-token" hx-swap="innerHTML"
            hx-swap-oob=[oob.then_some("true")] class="mt-4 grid gap-4" {
            label class="grid gap-1" {
                span class="font-semibold" { "Nome do dispositivo" }
                input class="field" type="text" name="name" required maxlength="64" placeholder="notebook" autocomplete="off";
            }
            button type="submit" class="btn" { "Criar token" }
        }
    }
}

fn token_row(t: &TokenRecord, online: bool) -> Markup {
    let dom = format!("tok-{}", t.id);
    html! {
        li id=(dom) class="grid grid-cols-[1fr_auto] items-center gap-x-4 gap-y-1 border-b border-ink/15 py-4 sm:grid-cols-[minmax(0,1fr)_9rem_auto]" {
            span class="min-w-0" {
                span class="block truncate text-lead font-semibold" { (t.name) }
                span class="block text-sm tabular-nums text-muted" { (t.id) " · criado em " (format_date(t.created_at)) }
            }
            span class="flex items-center gap-2" {
                span class=(if online { "dot dot-on" } else { "dot" }) aria-hidden="true" {}
                (if online { "Conectado" } else { "Desconectado" })
            }
            button type="button" class="btn btn-quiet justify-self-start !px-0 sm:justify-self-end sm:!px-3" hx-delete=(format!("/tokens/{}", t.id))
                hx-target=(format!("#{dom}")) hx-swap="outerHTML"
                hx-confirm=(format!("Revogar o token \"{}\"? O túnel que usa esse token cai em até 10 segundos.", t.name)) {
                "Revogar"
            }
        }
    }
}

/// Resposta do `POST /tokens` com sucesso: a bandeja com o comando (a única vez
/// que o token aparece), mais a linha nova na lista e o formulário limpo, via
/// trocas "fora de banda" do htmx.
pub fn token_created(rec: &TokenRecord, command: &str) -> Markup {
    html! {
        div data-ui class="ui-dark grid gap-3 p-5 sm:p-6" {
            p class="font-display text-lead font-bold leading-none tracking-tighter" { "Token criado." }
            p { "Ele aparece só agora. Copie o comando antes de sair desta página." }
            pre class="code" { code id="new-command" { (command) } }
            div { button type="button" class="btn !bg-sky !text-ink" data-copy="#new-command" { "Copiar comando" } }
        }
        ul id="token-rows" hx-swap-oob="beforeend" { (token_row(rec, false)) }
        p id="empty" hx-swap-oob="delete" {}
        (create_form(true))
    }
}

pub fn token_error(msg: &str) -> Markup {
    html! { p class="notice" role="alert" { (msg) } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_is_escaped() {
        let rec = TokenRecord {
            id: "a1b2c3d4".into(),
            name: "<script>alert(1)</script>\"x".into(),
            key: String::new(),
            created_at: 0,
            ports: None,
            subdomains: None,
            owner: None,
        };
        let html = dashboard("<b>e@x.com</b>", std::slice::from_ref(&rec), &[false]).into_string();
        assert!(!html.contains("<script>alert"), "nome do token escapado");
        assert!(!html.contains("<b>e@x.com"), "e-mail escapado");
        assert!(html.contains("&lt;script&gt;"));
        let fragment = token_created(&rec, "hbt configure --token a.b").into_string();
        assert!(!fragment.contains("<script>alert"));
    }

    #[test]
    fn login_error_and_email_are_rendered_escaped() {
        let html = login(Some(BAD_LOGIN), "a\"><img src=x>").into_string();
        assert!(html.contains(BAD_LOGIN));
        assert!(!html.contains("<img src=x>"));
    }

    #[test]
    fn closed_signup_shows_no_form_anywhere() {
        let html = signup(false, None, "").into_string();
        assert!(html.contains(SIGNUP_CLOSED));
        assert!(!html.contains("name=\"password\""));
        let landing = landing(false).into_string();
        assert!(
            !landing.contains("name=\"password\""),
            "a landing não oferece cadastro fechado"
        );
        assert!(!landing.contains("href=\"/signup\""));
    }

    #[test]
    fn landing_offers_signup_when_open() {
        let html = landing(true).into_string();
        assert!(html.contains("action=\"/signup\""));
        assert!(html.contains("Seu localhost, na internet."));
        assert!(html.contains(INSTALL));
    }

    #[test]
    fn pages_have_no_inline_script_or_style() {
        // O CSP não deixa nenhum dos dois: se aparecer um, o site quebra.
        for page in [
            landing(true),
            landing(false),
            login(None, ""),
            signup(true, None, ""),
            dashboard("a@b.co", &[], &[]),
        ] {
            let html = page.into_string();
            assert!(!html.contains("<style"), "estilo inline");
            assert!(!html.contains(" style=\""), "atributo style");
            assert!(!html.contains("onclick="), "handler inline");
            assert_eq!(html.matches("<script").count(), 2, "só app.js e htmx");
            assert_eq!(html.matches("<script src=").count(), 2);
        }
    }
}
