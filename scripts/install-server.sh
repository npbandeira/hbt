#!/usr/bin/env bash
# Instala o hbt-server como serviço systemd. Rode NO VPS (como root ou com sudo).
#
#   sudo ./install-server.sh --bin ./hbt-server-linux-x86_64-static --domain tunnel.exemplo.com
#
# Opções:
#   --bin PATH          binário do hbt-server (padrão: procura ao lado do script / em ../dist)
#   --domain D          domínio base dos túneis HTTP (habilita `hbt http`); mantém o anterior se omitido
#   --http-bind A       onde escutar HTTP        (padrão 0.0.0.0:80)
#   --https-bind A      onde escutar HTTPS, ex.: 0.0.0.0:443 (site no domínio raiz, túneis em *.domínio); mantém o anterior se omitido
#   --duckdns-token T   token do DuckDNS: emite e renova o certificado no Let's Encrypt (--domain <nome>.duckdns.org); mantém o anterior se omitido
#   --acme-email E      e-mail de contato no Let's Encrypt (opcional)
#   --control-bind A    porta de controle (UDP)   (padrão 0.0.0.0:7000)
#   --port-range A-B    portas públicas tcp/udp   (padrão 20000-30000)
#   --metrics-bind A    métricas Prometheus       (padrão 127.0.0.1:9477; "" desliga)
#   --web-bind A        site (cadastro e painel de tokens), ex.: 127.0.0.1:8081; mantém o anterior se omitido
#   --web-url U         URL pública do site, ex.: https://meuhbt.duckdns.org (com --https-bind já é esta por padrão)
#   --signup S          open (padrão) ou closed: quem pode criar conta no site
#
# Idempotente: rodar de novo atualiza o binário e reinicia, mantendo os tokens.
# Tokens e contas ficam em /var/lib/hbt/hbt.db (SQLite) e são gerenciados com
#   sudo -u hbt hbt-server token create|list|revoke
#   sudo -u hbt hbt-server user add|list|reset|disable|enable|remove
# Variável DESTDIR (como em pacotes) instala numa árvore de teste sem tocar no sistema.
set -euo pipefail

BIN="" DOMAIN="" HTTP_BIND="0.0.0.0:80" CONTROL_BIND="0.0.0.0:7000" RANGE="20000-30000" METRICS_BIND="127.0.0.1:9477" WEB_BIND="" WEB_URL="" SIGNUP="" HTTPS_BIND="" DUCKDNS_TOKEN="" ACME_EMAIL=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin) BIN="$2"; shift 2 ;;
    --domain) DOMAIN="$2"; shift 2 ;;
    --http-bind) HTTP_BIND="$2"; shift 2 ;;
    --https-bind) HTTPS_BIND="$2"; shift 2 ;;
    --duckdns-token) DUCKDNS_TOKEN="$2"; shift 2 ;;
    --acme-email) ACME_EMAIL="$2"; shift 2 ;;
    --control-bind) CONTROL_BIND="$2"; shift 2 ;;
    --port-range) RANGE="$2"; shift 2 ;;
    --metrics-bind) METRICS_BIND="$2"; shift 2 ;;
    --web-bind) WEB_BIND="$2"; shift 2 ;;
    --web-url) WEB_URL="$2"; shift 2 ;;
    --signup) SIGNUP="$2"; shift 2 ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "opção desconhecida: $1 (veja --help)" >&2; exit 2 ;;
  esac
done

die() { echo "erro: $*" >&2; exit 1; }

DESTDIR="${DESTDIR:-}"
if [[ -z "$DESTDIR" && $EUID -ne 0 ]]; then
  exec sudo -E bash "$0" "$@"          # reexecuta como root
fi
# Sem --bin procuramos o binário x86_64 do dist/; com --bin, quem chamou (ex.:
# install.sh) já escolheu o da arquitetura certa.
[[ -n "$BIN" || "$(uname -m)" == x86_64 || -n "$DESTDIR" ]] || die "binário estático é x86_64; esta máquina é $(uname -m) (use --bin)"

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
DATA_DIR="$DESTDIR/var/lib/hbt"
UNIT="$DESTDIR/etc/systemd/system/hbt-server.service"

# Lê um valor já existente no env (para manter o domínio em reinstalações).
existing() { [[ -f "$ENV_FILE" ]] && sed -n "s/^$1=//p" "$ENV_FILE" | head -n1 || true; }
[[ -n "$DOMAIN" ]] || DOMAIN="$(existing HBT_DOMAIN)"
[[ -n "$WEB_BIND" ]] || WEB_BIND="$(existing HBT_WEB_BIND)"
[[ -n "$WEB_URL" ]] || WEB_URL="$(existing HBT_WEB_URL)"
[[ -n "$SIGNUP" ]] || SIGNUP="$(existing HBT_SIGNUP)"
[[ -n "$HTTPS_BIND" ]] || HTTPS_BIND="$(existing HBT_HTTPS_BIND)"
[[ -n "$DUCKDNS_TOKEN" ]] || DUCKDNS_TOKEN="$(existing HBT_DUCKDNS_TOKEN)"
[[ -n "$ACME_EMAIL" ]] || ACME_EMAIL="$(existing HBT_ACME_EMAIL)"
[[ -z "$DUCKDNS_TOKEN" || -n "$HTTPS_BIND" ]] || HTTPS_BIND="0.0.0.0:443"
case "${SIGNUP:-open}" in open|closed) ;; *) die "--signup deve ser open ou closed" ;; esac

if [[ -z "$DESTDIR" ]]; then
  echo "==> usuário de sistema 'hbt' (sem shell, sem home)"
  id hbt >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin hbt
fi

echo "==> binário em $BIN_DST"
install -D -m 0755 "$BIN" "$BIN_DST"

echo "==> dados em $DATA_DIR (tokens e contas; só o usuário hbt acessa)"
if [[ -z "$DESTDIR" ]]; then
  install -d -m 0750 -o hbt -g hbt "$DATA_DIR"
else
  install -d -m 0750 "$DATA_DIR"
fi

echo "==> configuração em $ENV_FILE (só root lê: pode ter o token do DuckDNS)"
install -d -m 0755 "$ENV_DIR"
touch "$ENV_FILE" && chmod 0600 "$ENV_FILE"
{
  [[ -n "$DOMAIN" ]] && echo "HBT_DOMAIN=$DOMAIN"
  echo "HBT_HTTP_BIND=$HTTP_BIND"
  echo "HBT_DATA_DIR=/var/lib/hbt"
  if [[ -n "$METRICS_BIND" ]]; then echo "HBT_METRICS_BIND=$METRICS_BIND"; fi
  [[ -n "$WEB_BIND" ]] && echo "HBT_WEB_BIND=$WEB_BIND"
  [[ -n "$WEB_URL" ]] && echo "HBT_WEB_URL=$WEB_URL"
  [[ -n "$SIGNUP" ]] && echo "HBT_SIGNUP=$SIGNUP"
  [[ -n "$HTTPS_BIND" ]] && echo "HBT_HTTPS_BIND=$HTTPS_BIND"
  [[ -n "$DUCKDNS_TOKEN" ]] && echo "HBT_DUCKDNS_TOKEN=$DUCKDNS_TOKEN"
  [[ -n "$ACME_EMAIL" ]] && echo "HBT_ACME_EMAIL=$ACME_EMAIL"
  true
} > "$ENV_FILE"

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
ExecStart=/usr/local/bin/hbt-server serve --control-bind $CONTROL_BIND --port-range $RANGE
Restart=on-failure
RestartSec=2
# Muitos túneis = muitos sockets abertos.
LimitNOFILE=65536
# Permite escutar nas portas 80 e 443 sem rodar como root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
# Endurecimento: o serviço só precisa de rede e do diretório de dados.
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/hbt
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

CPORT="${CONTROL_BIND##*:}"; HPORT="${HTTP_BIND##*:}"; SPORT="${HTTPS_BIND##*:}"
echo
echo "hbt-server instalado e rodando."
if [[ -n "$DOMAIN" ]]; then
  echo "  HTTP : *.$DOMAIN -> porta $HPORT (precisa de DNS wildcard *.$DOMAIN -> IP deste servidor)"
  if [[ -n "$HTTPS_BIND" ]]; then
    echo "  HTTPS: $DOMAIN e *.$DOMAIN -> porta $SPORT; a porta $HPORT passa a redirecionar para https"
    if [[ -n "$DUCKDNS_TOKEN" ]]; then
      echo "         certificado emitido e renovado sozinho (DuckDNS + Let's Encrypt; leva ~2 min na primeira vez)"
    else
      echo "         ponha cert.pem e key.pem em /var/lib/hbt/tls/ (dono hbt) ou reinstale com --duckdns-token"
    fi
  fi
else
  echo "  HTTP : desabilitado (reinstale com --domain para habilitar 'hbt http')"
fi
[[ -n "$METRICS_BIND" ]] && echo "  Métricas: http://$METRICS_BIND/metrics  (na sua máquina: ./scripts/observability.sh up <host-ssh>)"
echo
# Gera o certificado na primeira instalação (como o usuário hbt, dono do diretório).
if [[ -n "$DESTDIR" ]]; then
  FINGERPRINT="$("$BIN_DST" --data-dir "$DATA_DIR" fingerprint)"
else
  FINGERPRINT="$(sudo -u hbt "$BIN_DST" fingerprint)"
fi
echo "Fingerprint do servidor (cada cliente confere este valor; vai no --fingerprint):"
echo "  $FINGERPRINT"
echo
if [[ -n "$WEB_BIND" ]]; then
  if [[ -n "$HTTPS_BIND" && -n "$DOMAIN" ]]; then
    echo "Site: https://$DOMAIN (cadastro ${SIGNUP:-open}); localmente também em http://$WEB_BIND."
  else
    echo "Site: $WEB_BIND (cadastro ${SIGNUP:-open}). Ele tem senha e token: sirva por HTTPS (--https-bind) ou ponha Caddy/nginx na frente."
  fi
  echo "  Contas: sudo -u hbt hbt-server user list|add|reset|disable|enable|remove"
  echo
fi
echo "Crie um token para cada cliente (mostrado uma única vez):"
echo "  sudo -u hbt hbt-server token create meu-notebook"
echo "  sudo -u hbt hbt-server token list"
echo "  sudo -u hbt hbt-server token revoke meu-notebook"
echo
echo "Libere no firewall do VPS (e no painel do provedor):"
HTTPS_RULE=""; HTTPS_UFW=""
if [[ -n "$HTTPS_BIND" ]]; then HTTPS_RULE="   ${SPORT}/tcp"; HTTPS_UFW=" && ufw allow ${SPORT}/tcp"; fi
echo "  ${CPORT}/udp   ${HPORT}/tcp${HTTPS_RULE}   ${RANGE/-/:}/tcp   ${RANGE/-/:}/udp"
if command -v ufw >/dev/null 2>&1; then
  echo "  ufw allow ${CPORT}/udp && ufw allow ${HPORT}/tcp${HTTPS_UFW} && ufw allow ${RANGE/-/:}/tcp && ufw allow ${RANGE/-/:}/udp"
fi
if [[ -n "$HTTPS_BIND" ]]; then
  echo "  (se havia Caddy/nginx na 443, pare-o antes: o hbt-server assume a porta)"
fi
echo
echo "Logs: journalctl -u hbt-server -f"
