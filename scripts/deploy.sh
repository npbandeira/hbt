#!/usr/bin/env bash
# Deploy completo em um comando, A PARTIR DA SUA MÁQUINA:
#   1. compila (se dist/ ainda não existe)
#   2. envia o binário e o instalador ao VPS por ssh/scp
#   3. instala o serviço systemd lá
#   4. cria um token exclusivo para esta máquina e configura o cliente local
#
#   ./scripts/deploy.sh usuario@meu.vps --domain tunnel.exemplo.com
#
# Opções: --domain D | --ssh-port N | --public-host H (nome/IP que o cliente usará;
# padrão: o host do ssh) | --no-configure | opções extras vão ao install-server.sh
set -euo pipefail
cd "$(dirname "$0")/.."

[[ $# -ge 1 && "$1" != -* ]] || { sed -n '2,12p' "$0"; exit 2; }
TARGET="$1"; shift
SSH_PORT=22 PUBLIC_HOST="${TARGET#*@}" CONFIGURE=1 EXTRA=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --ssh-port) SSH_PORT="$2"; shift 2 ;;
    --public-host) PUBLIC_HOST="$2"; shift 2 ;;
    --no-configure) CONFIGURE=0; shift ;;
    *) EXTRA+=("$1"); shift ;;
  esac
done

SSH=(ssh -p "$SSH_PORT" "$TARGET")
SCP=(scp -P "$SSH_PORT")

[[ -f dist/hbt-server-linux-x86_64-static ]] || ./scripts/build.sh

REMOTE_DIR="$("${SSH[@]}" 'mktemp -d')"
echo "==> enviando para $TARGET:$REMOTE_DIR"
"${SCP[@]}" dist/hbt-server-linux-x86_64-static scripts/install-server.sh "$TARGET:$REMOTE_DIR/"

echo "==> instalando no servidor (pode pedir a senha do sudo)"
# -t: terminal interativo para o sudo poder pedir senha.
ssh -t -p "$SSH_PORT" "$TARGET" \
  "sudo bash '$REMOTE_DIR/install-server.sh' --bin '$REMOTE_DIR/hbt-server-linux-x86_64-static' ${EXTRA[*]@Q}; rc=\$?; rm -rf '$REMOTE_DIR'; exit \$rc"

if [[ $CONFIGURE -eq 1 ]]; then
  echo "==> criando um token para este computador e configurando o cliente"
  # Um token por máquina (revogável individualmente), nomeado "usuario@host-data".
  NAME="$(whoami)@$(hostname -s)-$(date +%Y%m%d%H%M%S)"
  TOKEN="$("${SSH[@]}" "sudo -u hbt /usr/local/bin/hbt-server token create '$NAME' --quiet")"
  [[ -n "$TOKEN" ]] || { echo "não consegui criar o token no servidor" >&2; exit 1; }
  [[ -x "$HOME/.local/bin/hbt" ]] || ./scripts/install-client.sh
  HBT_SERVER="$PUBLIC_HOST" HBT_TOKEN="$TOKEN" "$HOME/.local/bin/hbt" configure
  echo
  echo "Token '$NAME' criado. Para revogar:  ssh $TARGET sudo -u hbt hbt-server token revoke '$NAME'"
  echo "Pronto. Teste:  hbt http 3000"
fi
