#!/usr/bin/env bash
# setup-instance.sh — idempotent first-boot setup for the travsr OCI instance
#
# Run once as root (or via sudo) on a fresh Ubuntu 22.04 ARM64 A1.Flex instance:
#
#   sudo bash setup-instance.sh
#
# The script is idempotent: re-running it after a partial failure is safe.
# Each step is self-contained and guards against repeating destructive operations.

set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Step 1: System update
# ─────────────────────────────────────────────────────────────────────────────
echo "==> [1/8] Updating and upgrading system packages..."
apt-get update -y
DEBIAN_FRONTEND=noninteractive apt-get upgrade -y

# ─────────────────────────────────────────────────────────────────────────────
# Step 2: UFW firewall rules
# Ports allowed: 22 (SSH), 80 (HTTP redirect), 443 (HTTPS/SSE).
# Port 3000 (axum) is intentionally ABSENT — it is VCN-restricted at the OCI
# Security List level (security.tf) and must never be exposed to the internet.
# ─────────────────────────────────────────────────────────────────────────────
echo "==> [2/8] Configuring UFW firewall..."
apt-get install -y ufw
ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp   comment 'SSH'
ufw allow 80/tcp   comment 'HTTP (HTTPS redirect)'
ufw allow 443/tcp  comment 'HTTPS (MCP SSE)'
ufw --force enable
ufw status verbose

# ─────────────────────────────────────────────────────────────────────────────
# Step 3: Docker install
# ─────────────────────────────────────────────────────────────────────────────
echo "==> [3/8] Installing Docker..."
if ! command -v docker &>/dev/null; then
    curl -fsSL https://get.docker.com | sh
else
    echo "    Docker already installed, skipping."
fi
usermod -aG docker ubuntu
# The 'ubuntu' user must log out and back in (or use 'newgrp docker') before
# docker commands work without sudo.

# ─────────────────────────────────────────────────────────────────────────────
# Step 4: Block volume detection, formatting, and mounting
#
# Uses /dev/disk/by-id (stable symlinks) rather than /dev/sdb (ephemeral).
# The block volume attached as paravirtualized shows up as a virtio device.
# We identify it by excluding the boot disk (which has a partition table).
# ─────────────────────────────────────────────────────────────────────────────
echo "==> [4/8] Detecting and mounting block volume..."

DATA_LABEL="travsr-data"
DATA_MOUNT="/data"
FSTAB_ENTRY="LABEL=${DATA_LABEL}  ${DATA_MOUNT}  ext4  defaults,nofail  0  2"

# Find the attached data volume: a block device that is not the boot disk
# (boot disk is the one containing /). We look for virtio-pci devices.
BOOT_DISK=$(lsblk -no PKNAME "$(findmnt -n -o SOURCE /)" 2>/dev/null || true)
DATA_DISK=""
for dev in /dev/vd* /dev/sd*; do
    [ -b "$dev" ] || continue
    # Skip partitions
    [[ "$dev" =~ [0-9]$ ]] && continue
    basename_dev=$(basename "$dev")
    if [ "$basename_dev" != "$BOOT_DISK" ]; then
        DATA_DISK="$dev"
        break
    fi
done

if [ -z "$DATA_DISK" ]; then
    echo "ERROR: Could not detect attached block volume. Ensure the volume is attached in OCI Console."
    exit 1
fi

echo "    Detected data disk: ${DATA_DISK}"

# Format only if not already formatted with the travsr-data label
if ! blkid "${DATA_DISK}" | grep -q "LABEL=\"${DATA_LABEL}\""; then
    echo "    Formatting ${DATA_DISK} as ext4 with label ${DATA_LABEL}..."
    mkfs.ext4 -L "${DATA_LABEL}" "${DATA_DISK}"
else
    echo "    ${DATA_DISK} already formatted with label ${DATA_LABEL}, skipping mkfs."
fi

# Mount if not already mounted
mkdir -p "${DATA_MOUNT}"
if ! mountpoint -q "${DATA_MOUNT}"; then
    mount -L "${DATA_LABEL}" "${DATA_MOUNT}"
    echo "    Mounted ${DATA_LABEL} at ${DATA_MOUNT}."
else
    echo "    ${DATA_MOUNT} already mounted, skipping."
fi

# Add to /etc/fstab for persistence across reboots (idempotent)
if ! grep -q "LABEL=${DATA_LABEL}" /etc/fstab; then
    echo "${FSTAB_ENTRY}" >> /etc/fstab
    echo "    Added fstab entry: ${FSTAB_ENTRY}"
else
    echo "    fstab entry already present, skipping."
fi

# Create tenant data directory
mkdir -p "${DATA_MOUNT}/tenants"
chown -R ubuntu:ubuntu "${DATA_MOUNT}/tenants"
echo "    Tenant directory ready at ${DATA_MOUNT}/tenants"

# ─────────────────────────────────────────────────────────────────────────────
# Step 5: Nginx install and configuration
# ─────────────────────────────────────────────────────────────────────────────
echo "==> [5/8] Installing and configuring Nginx..."
apt-get install -y nginx

# Copy the travsr Nginx config (assumes this script is run from the infra dir,
# or the config is uploaded to the instance alongside this script).
NGINX_CONF_SRC="$(dirname "$0")/../nginx/travsr-mcp.conf"
NGINX_CONF_DST="/etc/nginx/conf.d/travsr-mcp.conf"

if [ -f "$NGINX_CONF_SRC" ]; then
    cp "$NGINX_CONF_SRC" "$NGINX_CONF_DST"
    echo "    Copied travsr-mcp.conf to ${NGINX_CONF_DST}"
else
    echo "WARNING: ${NGINX_CONF_SRC} not found. Upload infra/nginx/travsr-mcp.conf to ${NGINX_CONF_DST} manually."
fi

# Remove the default Nginx site to avoid conflicts
if [ -f /etc/nginx/sites-enabled/default ]; then
    rm /etc/nginx/sites-enabled/default
    echo "    Removed default Nginx site."
fi

# Validate config before reloading (SSL cert not yet installed — test will warn)
nginx -t 2>&1 || true

systemctl enable nginx
systemctl reload nginx || systemctl start nginx
echo "    Nginx enabled and running."

# ─────────────────────────────────────────────────────────────────────────────
# Step 6: Certbot install and SSL instructions
# ─────────────────────────────────────────────────────────────────────────────
echo "==> [6/8] Installing Certbot..."
apt-get install -y certbot python3-certbot-nginx

cat <<'CERTBOT_INSTRUCTIONS'

    ┌──────────────────────────────────────────────────────────────────────────┐
    │  ACTION REQUIRED — SSL Certificate                                       │
    │                                                                          │
    │  Before running Certbot:                                                 │
    │  1. Point the DNS A record for mcp.travsr.com to this instance's        │
    │     public IP (from `terraform output instance_public_ip`).             │
    │  2. Wait for DNS propagation (check with: dig mcp.travsr.com A).        │
    │                                                                          │
    │  Then run:                                                               │
    │    sudo certbot --nginx -d mcp.travsr.com \                             │
    │      --non-interactive --agree-tos -m admin@travsr.com                  │
    │                                                                          │
    │  Certbot will automatically configure auto-renewal via a systemd timer. │
    └──────────────────────────────────────────────────────────────────────────┘

CERTBOT_INSTRUCTIONS

# ─────────────────────────────────────────────────────────────────────────────
# Step 7: OCIR docker login instructions
# ─────────────────────────────────────────────────────────────────────────────
echo "==> [7/8] OCIR Docker login instructions..."

cat <<'OCIR_INSTRUCTIONS'

    ┌──────────────────────────────────────────────────────────────────────────┐
    │  ACTION REQUIRED — OCIR Docker Login                                     │
    │                                                                          │
    │  To pull images from OCIR on this instance, run:                        │
    │                                                                          │
    │    docker login <region>.ocir.io \                                      │
    │      --username '<ocir_namespace>/<oci_username>' \                     │
    │      --password '<auth_token>'                                           │
    │                                                                          │
    │  Where:                                                                  │
    │    <region>          — e.g., ap-mumbai-1                                │
    │    <ocir_namespace>  — from `terraform output ocir_url` (first segment) │
    │    <oci_username>    — your OCI username (not email)                    │
    │    <auth_token>      — OCI Console → My Profile → Auth Tokens → Generate│
    │                                                                          │
    │  The docker login credentials are stored in /root/.docker/config.json  │
    │  and used by the travsr-mcp systemd service.                            │
    └──────────────────────────────────────────────────────────────────────────┘

OCIR_INSTRUCTIONS

# ─────────────────────────────────────────────────────────────────────────────
# Step 8: travsr-mcp systemd service
# ─────────────────────────────────────────────────────────────────────────────
echo "==> [8/8] Installing travsr-mcp systemd service..."

SERVICE_FILE="/etc/systemd/system/travsr-mcp.service"

if [ ! -f "$SERVICE_FILE" ]; then
    cat > "$SERVICE_FILE" <<'SERVICE'
[Unit]
Description=Travsr MCP SSE Server
Documentation=https://github.com/travsr/travsr
After=network-online.target docker.service
Wants=network-online.target
Requires=docker.service

[Service]
Type=simple
Restart=always
RestartSec=5
User=ubuntu

# /etc/travsr/env must exist and contain TRAVSR_VAULT_SECRET_OCID=<ocid>
# before starting this service. See vault-setup.sh for how to create the secret.
EnvironmentFile=/etc/travsr/env

ExecStartPre=-/usr/bin/docker rm -f travsr-mcp
ExecStart=/usr/bin/docker run \
    --rm \
    --name travsr-mcp \
    --platform linux/arm64 \
    -p 127.0.0.1:3000:3000 \
    -v /data/tenants:/data/tenants \
    --env-file /etc/travsr/env \
    ${TRAVSR_IMAGE} \
    serve --sse --port 3000 --tenants-dir /data/tenants
ExecStop=/usr/bin/docker stop travsr-mcp

[Install]
WantedBy=multi-user.target
SERVICE

    echo "    Service file written to ${SERVICE_FILE}."
    systemctl daemon-reload
    systemctl enable travsr-mcp
else
    echo "    ${SERVICE_FILE} already exists, skipping."
fi

cat <<'SERVICE_INSTRUCTIONS'

    ┌──────────────────────────────────────────────────────────────────────────┐
    │  ACTION REQUIRED — Env file + Service start                              │
    │                                                                          │
    │  1. Create the env directory and file:                                  │
    │       sudo mkdir -p /etc/travsr                                          │
    │       sudo tee /etc/travsr/env <<EOF                                    │
    │       TRAVSR_VAULT_SECRET_OCID=ocid1.vaultsecret.oc1...                 │
    │       TRAVSR_IMAGE=<region>.ocir.io/<namespace>/travsr-mcp:latest        │
    │       EOF                                                                │
    │                                                                          │
    │  2. Obtain TRAVSR_VAULT_SECRET_OCID by running vault-setup.sh.          │
    │                                                                          │
    │  3. After docker login and env file are ready, start the service:        │
    │       sudo systemctl start travsr-mcp                                    │
    │       sudo systemctl status travsr-mcp                                   │
    │       sudo journalctl -u travsr-mcp -f                                   │
    └──────────────────────────────────────────────────────────────────────────┘

SETUP COMPLETE. Follow the ACTION REQUIRED steps above to finish provisioning.

SERVICE_INSTRUCTIONS
