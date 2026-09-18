# Sourced by deploy-k3s.sh. Says what is wrong with an R2 key pair, if anything.
#
# R2's S3 API wants a 32-hex Access Key ID and a 64-hex Secret Access Key. The
# dashboard shows those two next to a third value, the API token itself
# (`cfut_…`, `cfat_…`), and next to the account ID in the endpoint URL. Paste the
# token and the account ID instead and nothing complains until restic runs:
# the backup job then failed every night for nine days before anyone looked.
# Checking the shape here turns that into a deploy that refuses to start.
#
#   r2_keys_problem "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
#
# Prints the reason and returns 1 when the pair cannot be R2 keys; prints
# nothing and returns 0 otherwise. Never prints either value.
r2_keys_problem() {
    local id=$1 secret=$2
    if [[ "$id" =~ ^cf[a-z]*_ ]]; then
        echo "AWS_ACCESS_KEY_ID holds a Cloudflare API token value; R2 wants the token's Access Key ID (32 hex characters)"
        return 1
    fi
    if ! [[ "$id" =~ ^[0-9a-fA-F]{32}$ ]]; then
        echo "AWS_ACCESS_KEY_ID is ${#id} characters, not an R2 Access Key ID (32 hex characters)"
        return 1
    fi
    if [[ "$secret" =~ ^[0-9a-fA-F]{32}$ ]]; then
        echo "AWS_SECRET_ACCESS_KEY is 32 hex characters, the shape of an account ID; R2's Secret Access Key is 64"
        return 1
    fi
    if ! [[ "$secret" =~ ^[0-9a-fA-F]{64}$ ]]; then
        echo "AWS_SECRET_ACCESS_KEY is ${#secret} characters, not an R2 Secret Access Key (64 hex characters)"
        return 1
    fi
    return 0
}
