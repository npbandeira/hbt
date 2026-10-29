#!/usr/bin/env bash
# Compila o hbt e deixa os binários prontos em dist/.
#   dist/hbt-server-linux-x86_64-static   -> servidor no VPS (estático: roda em qualquer Linux x86_64)
#   dist/hbt-linux-x86_64-static          -> cliente estático
#   dist/hbt, dist/hbt-server             -> versões nativas desta máquina
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-unknown-linux-musl

command -v cargo >/dev/null || { echo "erro: cargo não encontrado (instale via https://rustup.rs)" >&2; exit 1; }

echo "==> testes"
cargo test --workspace --quiet

echo "==> build nativo (release)"
cargo build --release --quiet

echo "==> build estático ($TARGET)"
rustup target add "$TARGET" >/dev/null 2>&1 || true
# O ring (QUIC) e o SQLite embutido compilam C, então o alvo musl precisa de um
# compilador C para musl (no Debian/Ubuntu: sudo apt install musl-tools).
if command -v musl-gcc >/dev/null; then
  export CC_x86_64_unknown_linux_musl=musl-gcc
else
  echo "aviso: musl-gcc não encontrado (sudo apt install musl-tools). Usando o gcc do host com flags de contorno:" >&2
  echo "       funciona (testado), mas compila o C contra os headers do glibc e não é o caminho suportado." >&2
  export CC_x86_64_unknown_linux_musl=gcc
  export CFLAGS_x86_64_unknown_linux_musl="-U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 -DSQLITE_DISABLE_LFS"
fi
cargo build --release --quiet --target "$TARGET"

mkdir -p dist
cp target/release/hbt target/release/hbt-server dist/
cp "target/$TARGET/release/hbt" dist/hbt-linux-x86_64-static
cp "target/$TARGET/release/hbt-server" dist/hbt-server-linux-x86_64-static
strip dist/* 2>/dev/null || true

echo "==> pronto:"
ls -lh dist
