// Carrega no <head>, sem defer: marca html.js antes da primeira pintura. As entradas
// (.reveal) só escondem o conteúdo quando há JavaScript para mostrá-lo.
document.documentElement.classList.add("js");

// ------------------------------------------------------------------ copiar
document.addEventListener("click", async (e) => {
  const btn = e.target.closest("[data-copy]");
  if (!btn) return;
  const source = document.querySelector(btn.dataset.copy);
  if (!source) return;
  const label = btn.textContent;
  try {
    await navigator.clipboard.writeText(source.textContent.trim());
    btn.textContent = "Copiado";
  } catch {
    // Sem permissão de área de transferência: seleciona o texto para Ctrl+C.
    getSelection().selectAllChildren(source);
    btn.textContent = "Selecionado: use Ctrl+C";
  }
  setTimeout(() => (btn.textContent = label), 2000);
});

document.addEventListener("DOMContentLoaded", () => {
  // ------------------------------------------- entradas ao chegar na tela
  const reveals = document.querySelectorAll(".reveal");
  if ("IntersectionObserver" in window) {
    const seen = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (!e.isIntersecting) continue;
          e.target.classList.add("in");
          seen.unobserve(e.target);
        }
      },
      { rootMargin: "0px 0px -8% 0px", threshold: 0.08 },
    );
    reveals.forEach((el) => seen.observe(el));
  } else {
    reveals.forEach((el) => el.classList.add("in"));
  }

  // ------------------------- construtor de comando (a "coisa viva" da página)
  const builder = document.getElementById("builder");
  if (builder) {
    const port = builder.querySelector("#builder-port");
    const cmd = builder.querySelector("#builder-cmd");
    const addr = builder.querySelector("#builder-addr");
    const tabs = [...builder.querySelectorAll("[data-kind]")];
    const defaults = { http: "3000", tcp: "22", udp: "51820" };
    const host = location.hostname || "seu-servidor";
    let kind = "http";
    let touched = false; // o usuário já digitou uma porta própria?

    const render = () => {
      const p = (port.value.match(/\d+/) || [defaults[kind]])[0];
      cmd.textContent = `hbt ${kind} ${p}`;
      addr.textContent =
        kind === "http" ? `http://app.${host}` : `${kind}://${host}:2xxxx`;
      tabs.forEach((t) => t.setAttribute("aria-selected", String(t.dataset.kind === kind)));
    };
    tabs.forEach((t) =>
      t.addEventListener("click", () => {
        kind = t.dataset.kind;
        if (!touched) port.value = defaults[kind];
        render();
      }),
    );
    port.addEventListener("input", () => {
      port.value = port.value.replace(/\D/g, "").slice(0, 5);
      touched = true;
      render();
    });
    render();
  }

  // -------------------- seletor dos três túneis (mostra um painel por vez)
  const kinds = document.getElementById("kinds");
  if (kinds) {
    const buttons = [...kinds.querySelectorAll("[data-show]")];
    const panels = [...document.querySelectorAll("#kind-panels > [id]")];
    const facts = [...document.querySelectorAll("#kind-facts > [data-fact]")];
    const host = location.hostname;
    if (host) {
      // Com o endereço real do servidor, os exemplos deixam de ser genéricos.
      panels.forEach((p) => {
        p.querySelectorAll("code").forEach((c) => {
          c.textContent = c.textContent.replaceAll("seu-servidor", host);
        });
      });
    }
    const show = (id) => {
      buttons.forEach((b) => b.setAttribute("aria-selected", String(b.dataset.show === id)));
      panels.forEach((p) => (p.hidden = p.id !== id));
      facts.forEach((f) => (f.hidden = f.dataset.fact !== id));
    };
    buttons.forEach((b) => b.addEventListener("click", () => show(b.dataset.show)));
    // Setas esquerda/direita entre as abas (padrão de teclado de abas).
    kinds.addEventListener("keydown", (e) => {
      if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
      const i = buttons.findIndex((b) => b.getAttribute("aria-selected") === "true");
      const next = buttons[(i + (e.key === "ArrowRight" ? 1 : buttons.length - 1)) % buttons.length];
      next.focus();
      show(next.dataset.show);
    });
    show("show-http");
  }
});
