#!/usr/bin/env bash
# Hook PostToolUse (Edit|Write) do Claude Code: depois de editar um .rs, formata
# o workspace e roda o clippy com as mesmas regras do CI. Erros do clippy voltam
# para o Claude (exit 2) para ele corrigir na hora; a edição em si já aconteceu.
set -uo pipefail

file="$(jq -r '.tool_input.file_path // .tool_response.filePath // empty')"
[[ "$file" == *.rs ]] || exit 0

cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}" || exit 0

if ! out="$(cargo fmt --all 2>&1)"; then
  echo "cargo fmt falhou (provável erro de sintaxe em $file):" >&2
  echo "$out" | head -n 40 >&2
  exit 2
fi

if ! out="$(cargo clippy --workspace --all-targets --quiet --color never -- -D warnings 2>&1)"; then
  echo "cargo clippy encontrou problemas depois de editar $file:" >&2
  echo "$out" | grep -v '^\s*$' | head -n 60 >&2
  exit 2
fi
exit 0
