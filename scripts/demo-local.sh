#!/usr/bin/env bash
# Demonstração completa NESTA máquina (sem VPS, sem DNS, sem root):
#   servidor + site de exemplo + clientes, mostrando túnel HTTP, túnel TCP,
#   limites por token e revogação ao vivo. Nada é instalado: tudo roda em um
#   diretório temporário e é desligado ao final.
#
#   ./scripts/demo-local.sh            # roda a demonstração e limpa tudo
#   ./scripts/demo-local.sh --keep     # deixa servidor+token prontos para você brincar (Ctrl+C encerra)
set -euo pipefail
cd "$(dirname "$0")/.."

KEEP=0
[[ "${1:-}" == "--keep" ]] && KEEP=1

say()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
ok()   { printf '    \033[32m✔\033[0m %s\n' "$*"; }
fail() { printf '    \033[31m✘ %s\033[0m\n' "$*"; exit 1; }

# Porta livre pedida ao SO.
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }

say "compilando (release)"
cargo build --release -q
SERVER="$PWD/target/release/hbt-server"
HBT="$PWD/target/release/hbt"

W="$(mktemp -d)"
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do [[ -n "$p" ]] && kill "$p" 2>/dev/null || true; done
  rm -rf "$W"
}
trap cleanup EXIT
trap 'echo; exit 0' INT TERM

CONTROL=$(free_port); HTTP=$(free_port); SITE=$(free_port); LO=$(free_port); HI=$((LO + 50))
export XDG_CONFIG_HOME="$W/config"          # o ~/.config real não é tocado
export HBT_DATA_DIR="$W/data"
unset HBT_TOKEN HBT_SERVER HBT_DOMAIN
export HBT_FINGERPRINT="$("$SERVER" fingerprint)"   # o cliente só confia neste certificado

say "servidor (controle :$CONTROL, HTTP :$HTTP, domínio *.localhost, portas $LO-$HI)"
"$SERVER" serve --control-bind "127.0.0.1:$CONTROL" --tunnel-ip 127.0.0.1 \
  --port-range "$LO-$HI" --domain localhost --http-bind "127.0.0.1:$HTTP" >"$W/server.log" 2>&1 &
PIDS+=($!)

say "site de exemplo em http://127.0.0.1:$SITE"
mkdir -p "$W/site"; echo "<h1>Olá de dentro do túnel!</h1>" > "$W/site/index.html"
(cd "$W/site" && exec python3 -m http.server "$SITE" --bind 127.0.0.1) >/dev/null 2>&1 &
PIDS+=($!)

say "criando tokens (um por cliente)"
ANA=$("$SERVER" token create ana --subdomains ana,ana-dev --ports "$LO-$((LO + 5))" --quiet)
ADMIN=$("$SERVER" token create admin --quiet)
"$SERVER" token list
echo "    (o segredo não aparece na listagem nem no disco)"

# Aguarda o servidor abrir a porta de controle (UDP: não dá para testar com connect).
for _ in $(seq 50); do
  grep -q "controle em" "$W/server.log" 2>/dev/null && break
  sleep 0.1
done

# Roda `hbt ...` em segundo plano e espera o banner "Forwarding <url>".
# Uso: start_client LOGFILE args...   (define BANNER_URL e CLIENT_PID)
start_client() {
  local log="$1"; shift
  "$HBT" "$@" >"$log" 2>&1 &
  CLIENT_PID=$!
  PIDS+=("$CLIENT_PID")
  BANNER_URL=""
  for _ in $(seq 50); do
    BANNER_URL=$(sed -n 's/^Forwarding *\([^ ]*\).*/\1/p' "$log" | head -n1)
    [[ -n "$BANNER_URL" ]] && return 0
    kill -0 "$CLIENT_PID" 2>/dev/null || return 1
    sleep 0.1
  done
  return 1
}

say "1) túnel HTTP:  hbt http $SITE --subdomain ana   (token da Ana)"
"$HBT" configure --server "127.0.0.1:$CONTROL" --token "$ANA" >/dev/null
start_client "$W/ana-http.log" http "$SITE" --subdomain ana || { cat "$W/ana-http.log"; fail "cliente não conectou"; }
ANA_HTTP_PID=$CLIENT_PID
echo "    URL pública: $BANNER_URL"
BODY=$(curl -s --max-time 5 -H "Host: ana.localhost:$HTTP" "http://127.0.0.1:$HTTP/")
[[ "$BODY" == *"Olá de dentro do túnel"* ]] && ok "o site local respondeu pela URL pública: $BODY" || fail "resposta inesperada: $BODY"
CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H "Host: naoexiste.localhost:$HTTP" "http://127.0.0.1:$HTTP/")
[[ "$CODE" == 404 ]] && ok "subdomínio sem túnel -> 404" || fail "esperava 404, veio $CODE"

say "2) limite do token: a Ana só pode usar os subdomínios ana e ana-dev"
if RUST_LOG=warn timeout 3 "$HBT" http "$SITE" --subdomain roubado >"$W/denied.log" 2>&1; then :; fi
grep -m1 "recusou" "$W/denied.log" | sed 's/^/    servidor: /' || true
grep -q "recusou" "$W/denied.log" && ok "pedido de 'roubado' foi recusado" || fail "deveria ter sido recusado"

say "3) túnel TCP:  hbt tcp $SITE   (porta pública sorteada pelo servidor)"
start_client "$W/ana-tcp.log" tcp "$SITE" || { cat "$W/ana-tcp.log"; fail "túnel tcp não subiu"; }
echo "    endereço público: $BANNER_URL"
PORT="${BANNER_URL##*:}"
BODY=$(curl -s --max-time 5 "http://127.0.0.1:$PORT/")
[[ "$BODY" == *"Olá de dentro do túnel"* ]] && ok "o mesmo site respondeu pela porta $PORT (dentro da faixa da Ana: $LO-$((LO + 5)))" || fail "resposta inesperada: $BODY"
kill "$CLIENT_PID" 2>/dev/null || true

if [[ $KEEP -eq 1 ]]; then
  "$HBT" configure --server "127.0.0.1:$CONTROL" --token "$ADMIN" >/dev/null
  say "tudo no ar. Em OUTRO terminal, use (config isolada em $W/config):"
  cat <<MSG

    export XDG_CONFIG_HOME=$W/config
    $HBT http $SITE --subdomain meuapp          # depois: curl -H 'Host: meuapp.localhost:$HTTP' http://127.0.0.1:$HTTP/
    $HBT tcp $SITE
    HBT_DATA_DIR=$W/data $SERVER token list
    HBT_DATA_DIR=$W/data $SERVER token revoke ana

    Navegador: http://ana.localhost:$HTTP  (o túnel HTTP da Ana segue ativo)
    Logs do servidor: $W/server.log
    Ctrl+C para encerrar e limpar tudo.
MSG
  while true; do sleep 3600; done
fi

say "4) revogação ao vivo: hbt-server token revoke ana"
"$SERVER" token revoke ana
echo "    aguardando o servidor derrubar a sessão (até ~10s)..."
# O cliente do túnel HTTP deve ter desistido (autenticação recusada ao reconectar).
for _ in $(seq 400); do
  kill -0 "$ANA_HTTP_PID" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$ANA_HTTP_PID" 2>/dev/null; then fail "o cliente continuou conectado após a revogação"; fi
ok "sessão derrubada e cliente encerrado (autenticação recusada ao reconectar)"
CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -H "Host: ana.localhost:$HTTP" "http://127.0.0.1:$HTTP/")
[[ "$CODE" == 404 ]] && ok "a URL pública da Ana agora responde 404" || fail "esperava 404, veio $CODE"

say "concluído: tudo funcionou"
