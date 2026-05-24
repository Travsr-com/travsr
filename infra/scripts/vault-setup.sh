#!/usr/bin/env bash
# vault-setup.sh — OCI Vault setup for the travsr-mcp signing key
#
# This script documents the manual steps required to:
#   1. Create an OCI Vault and key
#   2. Generate a 256-bit random signing key
#   3. Store it as an OCI Vault secret
#   4. Print the secret OCID for use in /etc/travsr/env
#
# Prerequisites:
#   - OCI CLI installed and configured: https://docs.oracle.com/en-us/iaas/Content/API/SDKDocs/cliinstall.htm
#   - `oci` command available in PATH
#   - Operator has IAM permissions: manage vaults, manage keys, manage secrets
#
# Run on the OPERATOR'S LOCAL MACHINE (not on the OCI instance).
# The signing key material never leaves OCI Vault after this script runs.
#
# Usage:
#   bash vault-setup.sh <compartment_ocid> <region>
#
# Example:
#   bash vault-setup.sh ocid1.compartment.oc1..aaaa... ap-mumbai-1

set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Configuration — edit these if needed
# ─────────────────────────────────────────────────────────────────────────────
COMPARTMENT_OCID="${1:-}"
REGION="${2:-ap-mumbai-1}"

VAULT_NAME="travsr-vault"
KEY_NAME="travsr-mcp-signing-master"
SECRET_NAME="travsr-mcp-signing-key"
SECRET_NAME_PREV="travsr-mcp-signing-key-prev"

if [ -z "$COMPARTMENT_OCID" ]; then
    echo "Usage: $0 <compartment_ocid> [region]"
    echo "Example: $0 ocid1.compartment.oc1..aaaa... ap-mumbai-1"
    exit 1
fi

echo "==> [1/5] Creating OCI Vault (idempotent — skips if already exists)..."
# Check if vault already exists
EXISTING_VAULT=$(oci kms management vault list \
    --compartment-id "$COMPARTMENT_OCID" \
    --region "$REGION" \
    --lifecycle-state ACTIVE \
    --query "data[?\"display-name\"=='${VAULT_NAME}'].id | [0]" \
    --raw-output 2>/dev/null || echo "")

if [ -n "$EXISTING_VAULT" ] && [ "$EXISTING_VAULT" != "null" ]; then
    VAULT_OCID="$EXISTING_VAULT"
    echo "    Vault already exists: ${VAULT_OCID}"
else
    VAULT_OCID=$(oci kms management vault create \
        --compartment-id "$COMPARTMENT_OCID" \
        --region "$REGION" \
        --display-name "$VAULT_NAME" \
        --vault-type DEFAULT \
        --query 'data.id' \
        --raw-output)
    echo "    Created vault: ${VAULT_OCID}"

    echo "    Waiting for vault to become ACTIVE..."
    oci kms management vault get \
        --vault-id "$VAULT_OCID" \
        --region "$REGION" \
        --wait-for-state ACTIVE \
        --max-wait-seconds 120
fi

# Retrieve the vault's management and crypto endpoints
MGMT_ENDPOINT=$(oci kms management vault get \
    --vault-id "$VAULT_OCID" \
    --region "$REGION" \
    --query 'data."management-endpoint"' \
    --raw-output)

echo "    Management endpoint: ${MGMT_ENDPOINT}"

echo "==> [2/5] Creating AES-256 master key for secret encryption..."
EXISTING_KEY=$(oci kms management key list \
    --compartment-id "$COMPARTMENT_OCID" \
    --endpoint "$MGMT_ENDPOINT" \
    --lifecycle-state ENABLED \
    --query "data[?\"display-name\"=='${KEY_NAME}'].id | [0]" \
    --raw-output 2>/dev/null || echo "")

if [ -n "$EXISTING_KEY" ] && [ "$EXISTING_KEY" != "null" ]; then
    KEY_OCID="$EXISTING_KEY"
    echo "    Key already exists: ${KEY_OCID}"
else
    KEY_OCID=$(oci kms management key create \
        --compartment-id "$COMPARTMENT_OCID" \
        --endpoint "$MGMT_ENDPOINT" \
        --display-name "$KEY_NAME" \
        --key-shape '{"algorithm":"AES","length":32}' \
        --query 'data.id' \
        --raw-output)
    echo "    Created key: ${KEY_OCID}"
fi

echo "==> [3/5] Generating 256-bit random signing key material..."
# Generate 32 bytes of random data, base64-encoded for storage as a Vault secret.
# The travsr-mcp auth module decodes this from base64 at startup.
SIGNING_KEY_B64=$(openssl rand -base64 32)
echo "    Generated signing key (base64, 32 bytes). NOT printing key material."

echo "==> [4/5] Storing signing key as OCI Vault secret..."
# Check if secret already exists
EXISTING_SECRET=$(oci vault secret list \
    --compartment-id "$COMPARTMENT_OCID" \
    --region "$REGION" \
    --lifecycle-state ACTIVE \
    --query "data[?\"secret-name\"=='${SECRET_NAME}'].id | [0]" \
    --raw-output 2>/dev/null || echo "")

if [ -n "$EXISTING_SECRET" ] && [ "$EXISTING_SECRET" != "null" ]; then
    echo "    Secret '${SECRET_NAME}' already exists. To rotate, see Step 5 below."
    SECRET_OCID="$EXISTING_SECRET"
else
    # Encode the key content as base64 for the Vault secret content
    SECRET_CONTENT_B64=$(echo -n "$SIGNING_KEY_B64" | base64)
    SECRET_OCID=$(oci vault secret create-base64 \
        --compartment-id "$COMPARTMENT_OCID" \
        --region "$REGION" \
        --secret-name "$SECRET_NAME" \
        --vault-id "$VAULT_OCID" \
        --key-id "$KEY_OCID" \
        --secret-content-content "$SECRET_CONTENT_B64" \
        --description "travsr-mcp HMAC-SHA256 signing key (current)" \
        --query 'data.id' \
        --raw-output)
    echo "    Created secret: ${SECRET_OCID}"
fi

echo ""
echo "═══════════════════════════════════════════════════════════════════════"
echo " SECRET OCID (add to /etc/travsr/env on the OCI instance):"
echo ""
echo "   TRAVSR_VAULT_SECRET_OCID=${SECRET_OCID}"
echo ""
echo "═══════════════════════════════════════════════════════════════════════"

echo "==> [5/5] Key rotation procedure (reference — not executed now)"
cat <<'ROTATION_DOCS'

KEY ROTATION PROCEDURE (run every 30 days):
─────────────────────────────────────────────────────────────────────────────

Per RFC-007 §4, the old key remains valid for 24 hours after a new key is
activated. travsr-mcp loads up to two keys at startup:
  - travsr-mcp-signing-key      (current)
  - travsr-mcp-signing-key-prev (previous, if present in Vault)

Step 1: Rename the current secret to "prev"
  oci vault secret schedule-secret-deletion \  # do NOT delete yet
      --secret-id <current-secret-ocid> ...

  Instead: create a NEW version of travsr-mcp-signing-key-prev with the
  current key's content:

  CURRENT_KEY=$(oci vault secret get-secret-bundle \
      --secret-id <TRAVSR_VAULT_SECRET_OCID> \
      --region ap-mumbai-1 \
      --query 'data."secret-bundle-content".content' \
      --raw-output)

  oci vault secret update-base64 \
      --secret-id <PREV_SECRET_OCID> \
      --secret-content-content "$CURRENT_KEY"

Step 2: Generate a new signing key and update travsr-mcp-signing-key
  NEW_KEY=$(openssl rand -base64 32 | base64)
  oci vault secret update-base64 \
      --secret-id <TRAVSR_VAULT_SECRET_OCID> \
      --secret-content-content "$NEW_KEY"

Step 3: Restart travsr-mcp on the OCI instance to pick up the new key
  ssh ubuntu@<instance_ip> 'sudo systemctl restart travsr-mcp'

Step 4: After 24 hours, delete the old version of travsr-mcp-signing-key-prev
  oci vault secret schedule-secret-deletion \
      --secret-id <PREV_SECRET_OCID> \
      --time-of-deletion "$(date -u -v+1d '+%Y-%m-%dT%H:%M:%SZ')"

Note: Token revocation is undefined until RFC-008 is accepted. After RFC-008,
update this rotation procedure accordingly.
─────────────────────────────────────────────────────────────────────────────

ROTATION_DOCS

echo "vault-setup.sh complete."
