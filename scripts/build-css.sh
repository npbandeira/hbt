#!/usr/bin/env bash
# Gera server/assets/app.css com o Tailwind (CLI standalone, sem Node).
#   ./scripts/build-css.sh           # gera uma vez
#   ./scripts/build-css.sh --watch   # regera ao editar server/src/web/ ou tailwind.css
#
# O app.css gerado é commitado: `cargo build` não precisa do Tailwind. Rode este
# script e commite o resultado sempre que mudar classes em server/src/web/ ou o
# server/assets/tailwind.css.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=v4.1.11
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ASSET=tailwindcss-linux-x64 ;;
  Linux-aarch64) ASSET=tailwindcss-linux-arm64 ;;
  Darwin-arm64) ASSET=tailwindcss-macos-arm64 ;;
  Darwin-x86_64) ASSET=tailwindcss-macos-x64 ;;
  *) echo "erro: sistema sem binário do Tailwind ($(uname -s)-$(uname -m))" >&2; exit 1 ;;
esac

BIN=".cache/tailwindcss-$VERSION"
if [[ ! -x "$BIN" ]]; then
  mkdir -p .cache
  echo "==> baixando o Tailwind $VERSION ($ASSET)"
  curl -fsSL -o "$BIN.tmp" "https://github.com/tailwindlabs/tailwindcss/releases/download/$VERSION/$ASSET"
  chmod +x "$BIN.tmp"
  mv "$BIN.tmp" "$BIN"
fi

"$BIN" -i server/assets/tailwind.css -o server/assets/app.css --minify "$@"
