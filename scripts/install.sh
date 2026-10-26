#!/bin/sh
# Instala o hbt a partir dos releases do GitHub.
#
#   Cliente (Linux/macOS), em ~/.local/bin:
#     curl -fsSL https://github.com/npbandeira/hbt/releases/latest/download/install.sh | sh
#     curl -fsSL .../install.sh | sh -s -- --server-addr meu.vps --token ID.SEGREDO   # já configura
#
#   Servidor (Linux, serviço systemd; pede sudo):
#     curl -fsSL .../install.sh | sh -s -- --server --domain tunnel.exemplo.com
#
# Opções:
#   --server            instala o hbt-server (as demais opções vão para o install-server.sh)
#   --version vX.Y.Z    versão específica (padrão: a mais recente)
#   --dir DIR           onde pôr o cliente (padrão: ~/.local/bin)
#   --server-addr HOST  e --token TOKEN: roda `hbt configure` depois de instalar
#
# Repositório privado: o download direto não funciona sem login; o script usa
# então o `gh` (GitHub CLI) já autenticado, se houver.
# HBT_REPO=dono/repo troca o repositório; HBT_DOWNLOAD_URL=URL baixa de um
# espelho (a URL deve conter os arquivos do release).
set -eu

REPO="${HBT_REPO:-npbandeira/hbt}"
VERSION=latest
DIR="${HOME}/.local/bin"
SERVER=0
ADDR=""
TOKEN=""

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[31merro:\033[0m %s\n' "$*" >&2; exit 1; }

# Opções do cliente ficam aqui; com --server, o resto vai para o install-server.sh.
while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER=1; shift ;;
    --version) VERSION="$2"; shift 2 ;;
    --dir) DIR="$2"; shift 2 ;;
    --server-addr) ADDR="$2"; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    -h|--help) sed -n '2,22p' "$0" 2>/dev/null || echo "veja o cabeçalho de install.sh"; exit 0 ;;
    *) [ "$SERVER" = 1 ] && break; die "opção desconhecida: $1 (veja --help)" ;;
  esac
done

case "$(uname -s)" in
  Linux) OS=linux ;;
  Darwin) OS=macos ;;
  *) die "sistema $(uname -s) não suportado por este script (no Windows, baixe hbt-windows-x86_64.exe do release)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "arquitetura $(uname -m) não suportada" ;;
esac
if [ "$SERVER" = 1 ] && [ "$OS" != linux ]; then
  die "o hbt-server roda em Linux (systemd)"
fi

command -v curl >/dev/null || die "precisa do curl"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Baixa um arquivo do release para $TMP: espelho, download direto (repo
# público) ou gh (repo privado, com login).
fetch() {
  name="$1"
  if [ -n "${HBT_DOWNLOAD_URL:-}" ]; then
    curl -fsSL "$HBT_DOWNLOAD_URL/$name" -o "$TMP/$name" && return 0
    die "não consegui baixar $HBT_DOWNLOAD_URL/$name"
  fi
  if [ "$VERSION" = latest ]; then
    url="https://github.com/$REPO/releases/latest/download/$name"
  else
    url="https://github.com/$REPO/releases/download/$VERSION/$name"
  fi
  curl -fsSL "$url" -o "$TMP/$name" 2>/dev/null && return 0
  if command -v gh >/dev/null 2>&1; then
    tag=""
    [ "$VERSION" = latest ] || tag="$VERSION"
    # shellcheck disable=SC2086 # tag vazio = release mais recente
    gh release download $tag -R "$REPO" -p "$name" -D "$TMP" --clobber 2>/dev/null && return 0
  fi
  die "não consegui baixar $name de $REPO ($VERSION). Repositório privado? Faça login com \`gh auth login\` (com acesso ao repo) e rode de novo."
}

# Confere o SHA-256 contra o SHA256SUMS do mesmo release.
verify() {
  name="$1"
  want="$(grep " $name\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)"
  [ -n "$want" ] || die "$name não está no SHA256SUMS"
  if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$TMP/$name" | cut -d' ' -f1)"
  else
    got="$(shasum -a 256 "$TMP/$name" | cut -d' ' -f1)"
  fi
  [ "$got" = "$want" ] || die "checksum de $name não confere (download corrompido ou adulterado)"
}

say "baixando hbt ($VERSION) de $REPO"
fetch SHA256SUMS

if [ "$SERVER" = 1 ]; then
  bin="hbt-server-$OS-$ARCH"
  fetch "$bin"; verify "$bin"
  fetch install-server.sh; verify install-server.sh
  chmod +x "$TMP/$bin"
  say "instalando o serviço (sudo)"
  sudo bash "$TMP/install-server.sh" --bin "$TMP/$bin" "$@"
  exit 0
fi

bin="hbt-$OS-$ARCH"
fetch "$bin"; verify "$bin"
mkdir -p "$DIR"
install -m 0755 "$TMP/$bin" "$DIR/hbt"
say "hbt $("$DIR/hbt" --version | cut -d' ' -f2) instalado em $DIR/hbt"

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "    atenção: $DIR não está no PATH; adicione ao seu shell: export PATH=\"$DIR:\$PATH\"" ;;
esac

if [ -n "$ADDR" ] || [ -n "$TOKEN" ]; then
  set --
  [ -n "$ADDR" ] && set -- "$@" --server "$ADDR"
  [ -n "$TOKEN" ] && set -- "$@" --token "$TOKEN"
  "$DIR/hbt" configure "$@"
else
  echo "    próximo passo: hbt configure --server SEU.VPS --token ID.SEGREDO"
fi
