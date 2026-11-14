#!/usr/bin/env bash
# Issue, revoke and list keys for the private archive (/archive).
#
# Usage:
#   ./archive_key.sh add <name>      issue a key and print the link to send
#   ./archive_key.sh revoke <name>   remove a key; that person is locked out
#                                    on their next request
#   ./archive_key.sh list            print who holds a key
#
# Keys live in data/archive_keys (mode 600), one "<name> <key>" per line. The
# server reads the file on every archive request, so nothing needs a restart.
# The link is the only copy of a key you hand out. Send it privately: anyone
# holding it can open the archive until you revoke it.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
keys_file="$script_dir/data/archive_keys"
origin="https://paulborrego.com"

usage() {
    sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

valid_name() {
    [[ "$1" =~ ^[A-Za-z0-9_-]+$ ]]
}

[[ $# -ge 1 ]] || usage
cmd="$1"

case "$cmd" in
add)
    [[ $# -eq 2 ]] || usage
    name="$2"
    if ! valid_name "$name"; then
        echo "error: name must be letters, digits, '-' or '_': $name" >&2
        exit 2
    fi
    mkdir -p "$(dirname "$keys_file")"
    umask 077
    touch "$keys_file"
    chmod 600 "$keys_file"
    if awk -v n="$name" '$1 == n { found = 1 } END { exit !found }' "$keys_file"; then
        echo "error: $name already has a key; revoke it first to issue a new one" >&2
        exit 1
    fi
    # 128 random bits, URL-safe base64 without padding: 22 characters.
    key="$(head -c 16 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n')"
    printf '%s %s\n' "$name" "$key" >> "$keys_file"
    echo "issued a key for $name. Send them this link:"
    echo
    echo "  $origin/archive?k=$key"
    ;;
revoke)
    [[ $# -eq 2 ]] || usage
    name="$2"
    if [[ ! -f "$keys_file" ]] || ! awk -v n="$name" '$1 == n { found = 1 } END { exit !found }' "$keys_file"; then
        echo "error: no key issued for $name" >&2
        exit 1
    fi
    tmp="$(mktemp "$keys_file.XXXXXX")"
    awk -v n="$name" '$1 != n' "$keys_file" > "$tmp"
    chmod 600 "$tmp"
    mv "$tmp" "$keys_file"
    echo "revoked $name"
    ;;
list)
    [[ -f "$keys_file" ]] || { echo "no keys issued"; exit 0; }
    awk '!/^#/ && NF >= 2 { print $1 }' "$keys_file"
    ;;
*)
    usage
    ;;
esac
