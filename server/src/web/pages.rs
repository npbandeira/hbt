//! HTML das páginas. As classes são do Tailwind (tema em `assets/tailwind.css`,
//! que também define os componentes `btn`, `field`, `tray`, `notice` e `steps`).

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

#[derive(Clone, Copy)]
enum Nav<'a> {
    Visitor,
    User(&'a str),
}

fn shell(title: &str, nav: Nav<'_>, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="pt-BR" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                meta name="htmx-config" content=(HTMX_CONFIG);
                title { (title) " · hbt" }
                link rel="stylesheet" href=(assets::url("app.css"));
                script src=(assets::url("htmx.min.js")) defer {}
                script src=(assets::url("app.js")) defer {}
            }
            body class="min-h-screen bg-concrete font-serif text-ink antialiased" {
                header class="mx-auto flex max-w-5xl items-center justify-between gap-4 px-4 py-5 sm:px-6" {
                    a href="/" class="font-mono text-xl font-bold tracking-tight" { "hbt" }
                    nav class="flex items-center gap-5 text-base" {
                        @match nav {
                            Nav::Visitor => {
                                a href="/login" class="underline-offset-4 hover:underline" { "Entrar" }
                                a href="/signup" class="btn btn-primary" { "Criar conta" }
                            }
                            Nav::User(email) => {
                                span class="hidden text-muted sm:inline" { (email) }
                                a href="/dashboard" class="underline-offset-4 hover:underline" { "Painel" }
                                form method="post" action="/logout" {
                                    button type="submit" class="underline-offset-4 hover:underline cursor-pointer" { "Sair" }
                                }
                            }
                        }
                    }
                }
                main class="mx-auto max-w-5xl px-4 pb-20 sm:px-6" { (body) }
                footer class="mx-auto max-w-5xl px-4 pb-10 text-sm text-muted sm:px-6" {
                    "hbt-server " (env!("CARGO_PKG_VERSION"))
                }
            }
        }
    }
}

// ---------------------------------------------------------------- landing

/// O painel da landing: porta pública ligada à porta local por um cabo que se
/// desenha uma vez ao abrir a página (veja `.cable` e `.led` no CSS).
fn panel_diagram() -> Markup {
    html! {
        svg viewBox="0 0 480 270" class="w-full" role="img"
            aria-label="Porta pública 24817 ligada por um cabo à porta local 3000" {
            rect x="1" y="1" width="478" height="268" rx="6" class="fill-panel stroke-line" stroke-width="2" {}
            // Portas: anel de latão e entrada escura.
            g {
                circle cx="96" cy="128" r="46" class="fill-concrete stroke-brass" stroke-width="5" {}
                circle cx="96" cy="128" r="26" class="fill-ink" {}
                circle cx="384" cy="128" r="46" class="fill-concrete stroke-brass" stroke-width="5" {}
                circle cx="384" cy="128" r="26" class="fill-ink" {}
            }
            text x="96" y="46" text-anchor="middle" class="fill-muted font-serif text-base" { "público" }
            text x="96" y="78" text-anchor="middle" class="fill-ink font-mono text-2xl font-bold" { "24817" }
            text x="384" y="46" text-anchor="middle" class="fill-muted font-serif text-base" { "local" }
            text x="384" y="78" text-anchor="middle" class="fill-ink font-mono text-2xl font-bold" { "3000" }
            // Cabo: pathLength=1 normaliza o comprimento para a animação.
            path d="M96 174 C 96 262, 384 262, 384 174" pathLength="1" fill="none"
                class="cable stroke-cable" stroke-width="9" stroke-linecap="round" {}
            g class="led" {
                circle cx="204" cy="128" r="6" class="fill-led" {}
                text x="218" y="133" class="fill-ink font-serif text-sm" { "conectado" }
            }
        }
    }
}

pub fn landing() -> Markup {
    shell(
        "Túnel reverso",
        Nav::Visitor,
        &html! {
            section class="grid items-center gap-10 py-10 lg:grid-cols-[1.05fr_1fr] lg:py-16" {
                div {
                    h1 class="max-w-[16ch] text-4xl font-semibold leading-[1.08] tracking-tight sm:text-6xl" {
                        "Sua máquina atrás do NAT, numa porta pública."
                    }
                    p class="mt-6 max-w-[42ch] text-lg leading-relaxed text-muted" {
                        "Crie uma conta, gere um token e cole um comando. O hbt abre a porta no servidor e leva o tráfego até o seu computador."
                    }
                    div class="mt-8 flex flex-wrap gap-3" {
                        a href="/signup" class="btn btn-primary" { "Criar conta" }
                        a href="#instalar" class="btn" { "Ver instalação" }
                    }
                }
                div { (panel_diagram()) }
            }

            section class="border-t border-line py-12" {
                h2 class="text-2xl font-semibold" { "Como começar" }
                ol class="steps mt-8 grid gap-8 sm:grid-cols-2 lg:grid-cols-4" {
                    li { h3 class="font-semibold" { "Crie a conta" }
                         p class="mt-1 text-muted" { "Com e-mail e senha, neste site." } }
                    li { h3 class="font-semibold" { "Crie um token" }
                         p class="mt-1 text-muted" { "No painel, dê um nome ao dispositivo." } }
                    li { h3 class="font-semibold" { "Cole o comando" }
                         p class="mt-1 text-muted" { "O painel mostra o " code class="font-mono text-sm" { "hbt configure" } " pronto." } }
                    li { h3 class="font-semibold" { "Abra o túnel" }
                         p class="mt-1 font-mono text-sm" { "hbt http 3000" } }
                }
            }

            section class="border-t border-line py-12" {
                h2 class="text-2xl font-semibold" { "Três tipos de túnel" }
                p class="mt-2 max-w-[60ch] text-muted" {
                    "Site, SSH, WireGuard, banco de dados: o que roda na sua máquina fica acessível de fora."
                }
                ul class="mt-8 divide-y divide-line border-y border-line" {
                    (tunnel_row("hbt http 3000", "Um endereço web só seu", "http://app.seu-servidor"))
                    (tunnel_row("hbt tcp 22", "Uma porta TCP pública", "tcp://seu-servidor:2xxxx"))
                    (tunnel_row("hbt udp 51820", "Uma porta UDP pública, sem atraso extra", "udp://seu-servidor:2xxxx"))
                }
            }

            section id="instalar" class="border-t border-line py-12" {
                h2 class="text-2xl font-semibold" { "Instalar" }
                p class="mt-2 max-w-[60ch] text-muted" {
                    "Linux e macOS. No Windows, baixe o " code class="font-mono text-sm" { "hbt-windows-x86_64.exe" }
                    " na página de releases."
                }
                div class="mt-6 max-w-2xl" {
                    (command_tray("install-cmd", "curl -fsSL https://github.com/npbandeira/hbt/releases/latest/download/install.sh | sh", "Copiar"))
                }
                h3 class="mt-12 text-lg font-semibold" { "O que protege a conexão" }
                p class="mt-2 max-w-[60ch] text-muted" {
                    "O cliente fala com o servidor por QUIC com TLS 1.3 e só aceita o certificado cujo fingerprint está no comando de configuração. O token nunca trafega: ele só assina um desafio."
                }
            }
        },
    )
}

fn tunnel_row(command: &str, what: &str, result: &str) -> Markup {
    html! {
        li class="grid gap-1 py-4 sm:grid-cols-[14rem_1fr_auto] sm:items-baseline sm:gap-6" {
            code class="font-mono text-base font-semibold" { (command) }
            span { (what) }
            span class="font-mono text-sm text-muted" { (result) }
        }
    }
}

/// Comando numa bandeja com botão de copiar (funciona sem JavaScript: o texto
/// fica selecionável).
fn command_tray(id: &str, command: &str, label: &str) -> Markup {
    html! {
        div class="flex flex-col gap-3 sm:flex-row sm:items-start" {
            pre class="tray min-w-0 flex-1" { code id=(id) class="select-all whitespace-pre-wrap break-all" { (command) } }
            button type="button" class="btn shrink-0" data-copy=(format!("#{id}")) { (label) }
        }
    }
}

// ------------------------------------------------------------ cadastro/login

/// O que muda entre as telas de entrar e de criar conta.
struct AuthForm<'a> {
    title: &'a str,
    action: &'a str,
    submit: &'a str,
    autocomplete: &'a str,
    email: &'a str,
    error: Option<&'a str>,
}

fn auth_page(f: &AuthForm<'_>, hint: &Markup, other: &Markup) -> Markup {
    html! {
        section class="max-w-sm py-8" {
            h1 class="text-3xl font-semibold tracking-tight" { (f.title) }
            @if let Some(msg) = f.error {
                p class="notice mt-6" role="alert" { (msg) }
            }
            form method="post" action=(f.action) hx-boost="true" class="mt-6 grid gap-5" {
                label class="grid gap-1" {
                    span class="text-sm font-semibold" { "E-mail" }
                    input class="field" type="email" name="email" value=(f.email) required autofocus autocomplete="email";
                }
                label class="grid gap-1" {
                    span class="text-sm font-semibold" { "Senha" }
                    input class="field" type="password" name="password" required autocomplete=(f.autocomplete) maxlength="256";
                    (hint)
                }
                button type="submit" class="btn btn-primary" { (f.submit) }
            }
            p class="mt-6 text-muted" { (other) }
        }
    }
}

pub fn login(error: Option<&str>, email: &str) -> Markup {
    let form = AuthForm {
        title: "Entrar",
        action: "/login",
        submit: "Entrar",
        autocomplete: "current-password",
        email,
        error,
    };
    shell(
        "Entrar",
        Nav::Visitor,
        &auth_page(
            &form,
            &html! {},
            &html! { "Ainda não tem conta? " a href="/signup" class="text-cable underline underline-offset-4" { "Criar conta" } },
        ),
    )
}

pub fn signup(open: bool, error: Option<&str>, email: &str) -> Markup {
    let form = AuthForm {
        title: "Criar conta",
        action: "/signup",
        submit: "Criar conta",
        autocomplete: "new-password",
        email,
        error,
    };
    let other = html! { "Já tem conta? " a href="/login" class="text-cable underline underline-offset-4" { "Entrar" } };
    let body = if open {
        auth_page(
            &form,
            &html! { span class="text-sm text-muted" { "Pelo menos 10 caracteres." } },
            &other,
        )
    } else {
        html! {
            section class="max-w-sm py-8" {
                h1 class="text-3xl font-semibold tracking-tight" { "Criar conta" }
                p class="notice mt-6" role="alert" { (SIGNUP_CLOSED) }
                p class="mt-6 text-muted" { (other) }
            }
        }
    };
    shell("Criar conta", Nav::Visitor, &body)
}

// ---------------------------------------------------------------- painel

pub fn dashboard(email: &str, tokens: &[TokenRecord], online: &[bool]) -> Markup {
    shell(
        "Painel",
        Nav::User(email),
        &html! {
            section class="py-8" {
                h1 class="text-3xl font-semibold tracking-tight" { "Seus tokens" }
                p class="mt-2 max-w-[60ch] text-muted" {
                    "Cada token autentica um computador. O comando de configuração aparece só no momento em que você cria o token."
                }

                ul id="token-rows" class="mt-8 divide-y divide-line border-y border-line" {
                    @for (t, on) in tokens.iter().zip(online) { (token_row(t, *on)) }
                }
                @if tokens.is_empty() {
                    p id="empty" class="mt-4 text-muted" { "Você ainda não tem tokens. Crie o primeiro abaixo." }
                }

                h2 class="mt-12 text-xl font-semibold" { "Criar token" }
                (create_form(false))
                div id="new-token" class="mt-6 max-w-2xl" aria-live="polite" {}
            }
        },
    )
}

fn create_form(oob: bool) -> Markup {
    html! {
        form id="create-form" hx-post="/tokens" hx-target="#new-token" hx-swap="innerHTML"
            hx-swap-oob=[oob.then_some("true")] class="mt-4 flex max-w-xl flex-col gap-3 sm:flex-row sm:items-end" {
            label class="grid flex-1 gap-1" {
                span class="text-sm font-semibold" { "Nome do dispositivo" }
                input class="field" type="text" name="name" required maxlength="64" placeholder="notebook" autocomplete="off";
            }
            button type="submit" class="btn btn-primary" { "Criar token" }
        }
    }
}

fn token_row(t: &TokenRecord, online: bool) -> Markup {
    let dom = format!("tok-{}", t.id);
    html! {
        li id=(dom) class="grid grid-cols-[auto_1fr_auto] items-center gap-x-4 gap-y-1 py-4 sm:grid-cols-[auto_1fr_auto_auto]" {
            span class=(if online { "lamp lamp-on" } else { "lamp" }) {
                span class="sr-only" { (if online { "Conectado" } else { "Desconectado" }) }
            }
            span class="min-w-0" {
                span class="block truncate font-semibold" { (t.name) }
                span class="block font-mono text-sm text-muted" { (t.id) }
            }
            span class="hidden text-sm text-muted sm:block" { "criado em " (format_date(t.created_at)) }
            button type="button" class="btn btn-quiet" hx-delete=(format!("/tokens/{}", t.id))
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
        div class="grid gap-3" {
            p class="font-semibold" { "Token criado" }
            p class="text-muted" { "Ele aparece só agora. Copie o comando antes de sair desta página." }
            (command_tray("new-command", command, "Copiar comando"))
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
    fn closed_signup_shows_no_form() {
        let html = signup(false, None, "").into_string();
        assert!(html.contains(SIGNUP_CLOSED));
        assert!(!html.contains("name=\"password\""));
    }

    #[test]
    fn pages_have_no_inline_script_or_style() {
        // O CSP não deixa nenhum dos dois: se aparecer um, o site quebra.
        for page in [
            landing(),
            login(None, ""),
            signup(true, None, ""),
            dashboard("a@b.co", &[], &[]),
        ] {
            let html = page.into_string();
            assert!(!html.contains("<style"), "estilo inline");
            assert!(!html.contains(" style=\""), "atributo style");
            assert!(!html.contains("onclick="), "handler inline");
            assert_eq!(html.matches("<script").count(), 2, "só htmx e app.js");
        }
    }
}
