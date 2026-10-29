// Botão "Copiar comando": copia o texto do elemento indicado em data-copy.
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
