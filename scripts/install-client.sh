#!/usr/bin/env bash
# Instala o cliente `hbt` na sua máquina e (opcionalmente) já configura.
#   ./scripts/install-client.sh --server tunnel.exemplo.com --token SEGREDO
#   ./scripts/install-client.sh                 # só instala
#
# Instala em ~/.local/bin (sem sudo). Use --system para /usr/local/bin.
set -euo pipefail
cd "$(dirname "$0")/.."

SERVER="" TOKEN="" DEST="$HOME/.local/bin"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --server) SERVER="$2"; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --system) DEST=/usr/local/bin; shift ;;
    -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
    *) echo "opção desconhecida: $1" >&2; exit 2 ;;
  esac
done

SRC=dist/hbt
[[ -f "$SRC" ]] || { echo "dist/hbt não existe: rode ./scripts/build.sh antes" >&2; exit 1; }

# Dentro do HOME nunca precisa de sudo (o diretório pode ainda não existir).
if [[ "$DEST" == "$HOME"/* || -w "$DEST" ]]; then
  install -D -m 0755 "$SRC" "$DEST/hbt"
else
  sudo install -D -m 0755 "$SRC" "$DEST/hbt"
fi
echo "hbt instalado em $DEST/hbt"
case ":$PATH:" in *":$DEST:"*) ;; *) echo "aviso: $DEST não está no seu PATH" ;; esac

if [[ -n "$SERVER" || -n "$TOKEN" ]]; then
  # Passa pelo ambiente? Não: argumentos aparecem em `ps` por instantes; para
  # um segredo, a env evita isso. O `configure` lê a env e grava no arquivo.
  HBT_SERVER="$SERVER" HBT_TOKEN="$TOKEN" "$DEST/hbt" configure
fi
echo "Uso: hbt http 3000"
