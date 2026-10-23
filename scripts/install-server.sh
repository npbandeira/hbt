#!/usr/bin/env bash
# Instala o hbt-server como serviço systemd. Rode NO VPS (como root ou com sudo).
#
#   sudo ./install-server.sh --bin ./hbt-server-linux-x86_64-static --domain tunnel.exemplo.com
#
# Opções:
#   --bin PATH          binário do hbt-server (padrão: procura ao lado do script / em ../dist)
#   --domain D          domínio base dos túneis HTTP (habilita `hbt http`); mantém o anterior se omitido
#   --http-bind A       onde escutar HTTP        (padrão 0.0.0.0:80)
#   --control-bind A    porta de controle         (padrão 0.0.0.0:7000)
#   --port-range A-B    portas públicas tcp/udp   (padrão 20000-30000)
#
# É idempotente: rodar de novo atualiza o binário e reinicia, MANTENDO o token.
# Variável DESTDIR (como em pacotes) instala numa árvore de teste sem tocar no sistema.
set -euo pipefail

BIN="" DOMAIN="" HTTP_BIND="0.0.0.0:80" CONTROL_BIND="0.0.0.0:7000" RANGE="20000-30000"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin) BIN="$2"; shift 2 ;;
    --domain) DOMAIN="$2"; shift 2 ;;
    --http-bind) HTTP_BIND="$2"; shift 2 ;;
    --control-bind) CONTROL_BIND="$2"; shift 2 ;;
    --port-range) RANGE="$2"; shift 2 ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "opção desconhecida: $1 (veja --help)" >&2; exit 2 ;;
  esac
done

die() { echo "erro: $*" >&2; exit 1; }

DESTDIR="${DESTDIR:-}"
if [[ -z "$DESTDIR" && $EUID -ne 0 ]]; then
  exec sudo -E bash "$0" "$@"          # reexecuta como root
fi
[[ "$(uname -m)" == x86_64 || -n "$DESTDIR" ]] || die "binário estático é x86_64; esta máquina é $(uname -m)"

if [[ -z "$BIN" ]]; then
  here="$(cd "$(dirname "$0")" && pwd)"
  for c in "$here/hbt-server-linux-x86_64-static" "$here/../dist/hbt-server-linux-x86_64-static"; do
    [[ -f "$c" ]] && BIN="$c" && break
  done
fi
[[ -n "$BIN" && -f "$BIN" ]] || die "binário do servidor não encontrado (use --bin)"

BIN_DST="$DESTDIR/usr/local/bin/hbt-server"
ENV_DIR="$DESTDIR/etc/hbt"
ENV_FILE="$ENV_DIR/server.env"
UNIT="$DESTDIR/etc/systemd/system/hbt-server.service"

# Lê um valor já existente no env (para manter token/domínio em reinstalações).
existing() { [[ -f "$ENV_FILE" ]] && sed -n "s/^$1=//p" "$ENV_FILE" | head -n1 || true; }

TOKEN="$(existing HBT_TOKEN)"
NEW_TOKEN=0
if [[ -z "$TOKEN" ]]; then
  TOKEN="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"   # 256 bits aleatórios
  NEW_TOKEN=1
fi
[[ -n "$DOMAIN" ]] || DOMAIN="$(existing HBT_DOMAIN)"

if [[ -z "$DESTDIR" ]]; then
  echo "==> usuário de sistema 'hbt' (sem shell, sem home)"
  id hbt >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin hbt
fi

echo "==> binário em $BIN_DST"
install -D -m 0755 "$BIN" "$BIN_DST"

echo "==> configuração em $ENV_FILE (somente root lê)"
install -d -m 0755 "$ENV_DIR"
umask 077   # o arquivo contém o token: nasce privado, sem janela de leitura
{
  echo "HBT_TOKEN=$TOKEN"
  [[ -n "$DOMAIN" ]] && echo "HBT_DOMAIN=$DOMAIN"
  echo "HBT_HTTP_BIND=$HTTP_BIND"
} > "$ENV_FILE"
umask 022

echo "==> serviço systemd"
install -d -m 0755 "$(dirname "$UNIT")"
cat > "$UNIT" <<UNIT_EOF
[Unit]
Description=hbt reverse tunnel server
After=network-online.target
Wants=network-online.target

[Service]
User=hbt
Group=hbt
EnvironmentFile=/etc/hbt/server.env
ExecStart=/usr/local/bin/hbt-server --control-bind $CONTROL_BIND --port-range $RANGE
Restart=on-failure
RestartSec=2
# Muitos túneis = muitos sockets abertos.
LimitNOFILE=65536
# Permite escutar na porta 80 sem rodar como root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
# Endurecimento: o serviço só precisa de rede.
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectControlGroups=yes
RestrictAddressFamilies=AF_INET AF_INET6

[Install]
WantedBy=multi-user.target
UNIT_EOF

if [[ -n "$DESTDIR" ]]; then
  echo "==> (DESTDIR=$DESTDIR) pulando systemctl"
else
  systemctl daemon-reload
  systemctl enable hbt-server >/dev/null 2>&1
  systemctl restart hbt-server
  sleep 1
  systemctl is-active --quiet hbt-server || { journalctl -u hbt-server -n 20 --no-pager; die "o serviço não subiu"; }
fi

CPORT="${CONTROL_BIND##*:}"; HPORT="${HTTP_BIND##*:}"
echo
echo "hbt-server instalado e rodando."
[[ -n "$DOMAIN" ]] && echo "  HTTP : *.$DOMAIN -> porta $HPORT (precisa de DNS wildcard *.$DOMAIN -> IP deste servidor)" \
                  || echo "  HTTP : desabilitado (reinstale com --domain para habilitar 'hbt http')"
echo "  Token: $TOKEN$([[ $NEW_TOKEN -eq 1 ]] && echo '  (novo)' || echo '  (mantido)')"
echo
echo "Libere no firewall do VPS (e no painel do provedor):"
echo "  ${CPORT}/tcp   ${HPORT}/tcp   ${RANGE/-/:}/tcp   ${RANGE/-/:}/udp"
if command -v ufw >/dev/null 2>&1; then
  echo "  ufw allow ${CPORT}/tcp && ufw allow ${HPORT}/tcp && ufw allow ${RANGE/-/:}/tcp && ufw allow ${RANGE/-/:}/udp"
fi
echo
echo "Logs: journalctl -u hbt-server -f"
