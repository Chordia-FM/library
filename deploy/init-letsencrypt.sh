#!/usr/bin/env bash
# One-time Let's Encrypt bootstrap for the OPTIONAL library edge overlay (compose.edge.yaml).
# Only needed if you front the library with nginx on a public VPS with your own domain. If you use a
# Cloudflare Tunnel instead, skip this entirely.
#
# Prereqs: DNS for LIBRARY_DOMAIN points at this host; ./.env exists with LIBRARY_DOMAIN + CERTBOT_EMAIL.
# Run from library/deploy:  bash init-letsencrypt.sh
set -euo pipefail
cd "$(dirname "$0")"

[ -f .env ] || { echo "ERROR: .env not found. Copy .env.example to .env first." >&2; exit 1; }
set -a; . ./.env; set +a

DOMAIN="${LIBRARY_DOMAIN:?set LIBRARY_DOMAIN in .env}"
EMAIL="${CERTBOT_EMAIL:?set CERTBOT_EMAIL in .env}"
STAGING="${CERTBOT_STAGING:-0}"

dc() { docker compose -f compose.prod.yaml -f compose.edge.yaml "$@"; }
p="/etc/letsencrypt/live/$DOMAIN"

echo "### [1/4] Dummy certificate so nginx can boot ..."
dc run --rm --entrypoint sh certbot -c \
  "mkdir -p '$p' && openssl req -x509 -nodes -newkey rsa:2048 -days 1 \
     -keyout '$p/privkey.pem' -out '$p/fullchain.pem' -subj '/CN=$DOMAIN'"

echo "### [2/4] Starting nginx ..."
dc up -d nginx

echo "### [3/4] Requesting the real certificate ..."
staging_flag=""
[ "$STAGING" != "0" ] && { staging_flag="--staging"; echo "    (using Let's Encrypt STAGING CA)"; }
dc run --rm --entrypoint sh certbot -c \
  "rm -rf '/etc/letsencrypt/live/$DOMAIN' '/etc/letsencrypt/archive/$DOMAIN' '/etc/letsencrypt/renewal/$DOMAIN.conf'"
# shellcheck disable=SC2086
dc run --rm --entrypoint certbot certbot certonly --webroot -w /var/www/certbot \
  --cert-name "$DOMAIN" -d "$DOMAIN" \
  --email "$EMAIL" --agree-tos --no-eff-email --non-interactive $staging_flag

echo "### [4/4] Reloading nginx ..."
dc exec nginx nginx -s reload
echo "Done. Certificate installed for $DOMAIN."
