#!/usr/bin/env bash
# Teste de carga com o servidor em Docker limitado a 1 CPU e 2 GB de RAM.
#   servidor (limitado) <── túneis ── clientes hbt ──► nginx / eco UDP   (tudo em containers)
#   oha          ──► servidor:8080 (HTTP) e servidor:20000 (TCP)
#   hbt-udpbench ──► servidor:20001 (UDP)
#
#   ./scripts/loadtest.sh                       # tudo: HTTP, TCP e UDP
#   MODES=udp ./scripts/loadtest.sh             # só UDP (MODES aceita: http tcp udp)
#   DURATION=60s CONC="50 1000" ./scripts/loadtest.sh
#   UDP_RATES="20000 200000" UDP_PEERS=50 UDP_SIZE=1200 ./scripts/loadtest.sh
#   ./scripts/loadtest.sh --keep                # deixa tudo no ar ao final
set -euo pipefail
cd "$(dirname "$0")/.."

MODES=${MODES:-"http tcp udp"}
DURATION=${DURATION:-20s}
CONC=${CONC:-"10 100 500"}
UDP_RATES=${UDP_RATES:-"10000 50000 100000"} # pacotes/s
UDP_PEERS=${UDP_PEERS:-10}                   # origens distintas (sessões no cliente)
UDP_SIZE=${UDP_SIZE:-512}                    # bytes por datagrama
UDP_MANY_PEERS=${UDP_MANY_PEERS:-1000}       # cenário extra: muitos peers a 10k pps
OHA=ghcr.io/hatoo/oha:1.16.0
KEEP=0
[[ "${1:-}" == "--keep" ]] && KEEP=1

say() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
dc() { docker compose -f docker/compose.loadtest.yml "$@"; }
want() { [[ " $MODES " == *" $1 "* ]]; }

OUT="$(mktemp -d)"
cleanup() {
  [[ -n "${STATS_PID:-}" ]] && kill "$STATS_PID" 2>/dev/null || true
  [[ $KEEP -eq 1 ]] || dc --profile clients down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

say "build das imagens e servidor (1 CPU, 2 GB)"
dc build -q
dc up -d --wait server backend udp-echo
NET="$(docker inspect -f '{{range $k, $_ := .NetworkSettings.Networks}}{{$k}}{{end}}' "$(dc ps -q server)")"
SERVER_CT="$(dc ps -q server)"
docker inspect -f '    limites: NanoCpus={{.HostConfig.NanoCpus}} Memory={{.HostConfig.Memory}}' "$SERVER_CT"

say "token e clientes"
HBT_TOKEN="$(dc exec -T server hbt-server token create "load-$RANDOM" --quiet)"
export HBT_TOKEN
dc --profile clients up -d client-http client-tcp client-udp

# O gerador também abre muitas conexões curtas: sem isso ele mesmo esgota portas (os error 99).
oha() {
  docker run --rm --network "$NET" --ulimit nofile=65536 \
    --sysctl net.ipv4.tcp_tw_reuse=1 --sysctl net.ipv4.ip_local_port_range="1024 65535" \
    "$OHA" --no-tui "$@"
}
udpbench() { docker run --rm --network "$NET" --ulimit nofile=65536 hbt:bench hbt-udpbench "$@"; }

# Espera os túneis responderem.
for _ in $(seq 50); do
  oha -n 1 -H 'Host: load.localhost' http://server:8080/ 2>/dev/null | grep -q '\[200\]' \
    && oha -n 1 http://server:20000/ 2>/dev/null | grep -q '\[200\]' && break
  sleep 0.3
done
udpbench ping --target server:20001 || echo "    aviso: túnel UDP não respondeu"

# Amostra CPU/memória do servidor durante todo o teste.
( while true; do
    docker stats --no-stream --format '{{.CPUPerc}} {{.MemUsage}}' "$SERVER_CT" | sed "s/^/$(date +%T) /"
  done ) >"$OUT/stats.log" 2>/dev/null &
STATS_PID=$!

# Pico de CPU do servidor desde a linha $1 do stats.log (= durante o último cenário).
mark() { wc -l <"$OUT/stats.log"; }
peak() { echo "    servidor: pico $(tail -n +"$(($1 + 1))" "$OUT/stats.log" | sort -k2 -n | tail -n1 | cut -d' ' -f2-)"; }

summary() { grep -E 'Success rate|Requests/sec|Slowest|Average|99.00%|\[[0-9]{3}\]|Error' "$1" | sed 's/^ */    /'; }
run_oha() { # nome título args...
  local name="$1" title="$2" m; shift 2
  say "$title  $DURATION"
  m=$(mark)
  oha -z "$DURATION" "$@" >"$OUT/$name.txt" 2>&1 || true
  summary "$OUT/$name.txt"
  peak "$m"
}
run_udp() { # nome título args...
  local name="$1" title="$2" m; shift 2
  say "$title  $DURATION"
  m=$(mark)
  udpbench load --duration "$DURATION" --size "$UDP_SIZE" "$@" 2>&1 | tee "$OUT/$name.txt" | sed 's/^ */    /'
  peak "$m"
}

for c in $CONC; do
  want http && run_oha "http-$c" "HTTP (subdomínio)  c=$c" -c "$c" -H 'Host: load.localhost' http://server:8080/
  want tcp && run_oha "tcp-$c" "TCP (porta 20000)  c=$c" -c "$c" http://server:20000/
  want http && run_oha "http-nokeep-$c" "HTTP com conexão nova por requisição (pior caso)  c=$c" \
    -c "$c" --disable-keepalive -H 'Host: load.localhost' http://server:8080/
done

if want udp; then
  top="${UDP_RATES##* }"
  run_udp udp-direct "UDP direto no eco, sem túnel (referência)  $top pps" \
    --target udp-echo:9000 --rate "$top" --peers "$UDP_PEERS"
  for r in $UDP_RATES; do
    run_udp "udp-$r" "UDP (porta 20001)  $r pps, $UDP_PEERS peers" \
      --target server:20001 --rate "$r" --peers "$UDP_PEERS"
  done
  run_udp udp-many "UDP com muitos peers  10000 pps, $UDP_MANY_PEERS peers" \
    --target server:20001 --rate 10000 --peers "$UDP_MANY_PEERS"
fi

kill "$STATS_PID" 2>/dev/null || true; STATS_PID=
say "servidor durante o teste (CPU% limite = 100%, memória)"
echo "    maior CPU:     $(sort -k2 -n "$OUT/stats.log" | tail -n1)"
echo "    maior memória: $(sort -k3 -h "$OUT/stats.log" | tail -n1)"
echo "    OOM kill: $(docker inspect -f '{{.State.OOMKilled}}' "$SERVER_CT")  reinícios: $(docker inspect -f '{{.RestartCount}}' "$SERVER_CT")"
echo "    resultados completos em $OUT"
[[ $KEEP -eq 1 ]] && echo "    no ar: docker compose -f docker/compose.loadtest.yml --profile clients down -v  para derrubar"
