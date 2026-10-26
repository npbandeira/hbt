#!/usr/bin/env bash
# Prometheus + Grafana NESTA máquina, lendo as métricas do hbt-server.
#
#   ./scripts/observability.sh up vps      # túnel SSH até o host "vps" + Prometheus + Grafana
#   ./scripts/observability.sh up          # só o stack (ex.: para o teste de carga local)
#   ./scripts/observability.sh status      # alvos do Prometheus e túnel SSH
#   ./scripts/observability.sh down        # para tudo (o histórico fica nos volumes)
#
# No VPS o hbt-server serve /metrics só em 127.0.0.1:9477 (sem porta aberta na
# internet); o `ssh -L` traz essa porta para 127.0.0.1:9477 aqui.
# Grafana: http://127.0.0.1:3000   Prometheus: http://127.0.0.1:9090
set -euo pipefail
cd "$(dirname "$0")/.."

FWD="127.0.0.1:9477:127.0.0.1:9477"
dc() { docker compose -f observability/compose.yml "$@"; }
tunnel_pid() { pgrep -f -- "-L $FWD" || true; }

case "${1:-}" in
  up)
    host="${2:-}"
    if [[ -n "$host" ]]; then
      if [[ -n "$(tunnel_pid)" ]]; then
        echo "==> túnel SSH já está no ar (pid $(tunnel_pid))"
      else
        echo "==> túnel SSH: $host:9477 -> 127.0.0.1:9477"
        # -f vai para segundo plano só depois de o encaminhamento funcionar.
        ssh -f -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
          -L "$FWD" "$host"
      fi
    fi
    echo "==> Prometheus + Grafana"
    dc up -d
    for _ in $(seq 60); do
      curl -fs http://127.0.0.1:3000/api/health >/dev/null 2>&1 && break
      sleep 0.5
    done
    "$0" status
    echo
    echo "Grafana:    http://127.0.0.1:3000   (dashboard \"hbt — túneis\")"
    echo "Prometheus: http://127.0.0.1:9090"
    ;;
  status)
    pid="$(tunnel_pid)"
    if [[ -n "$pid" ]]; then echo "túnel SSH: no ar (pid $pid)"; else echo "túnel SSH: desligado"; fi
    sleep 6 # um ciclo de scrape (5s) depois de subir
    curl -fs http://127.0.0.1:9090/api/v1/targets 2>/dev/null | python3 -c '
import json, sys
for t in json.load(sys.stdin)["data"]["activeTargets"]:
    inst, health, url = t["labels"]["instance"], t["health"], t["scrapeUrl"]
    err = (t.get("lastError") or "")[:60]
    print(f"  {inst:<9} {health:<7} {url}  {err}")
' || echo "  Prometheus fora do ar"
    ;;
  down)
    dc down
    pid="$(tunnel_pid)"
    if [[ -n "$pid" ]]; then kill $pid && echo "túnel SSH encerrado"; fi
    ;;
  *)
    sed -n '2,11p' "$0"
    exit 2
    ;;
esac
