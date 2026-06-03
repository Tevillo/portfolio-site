#!/usr/bin/env bash
# Set (or change) the download password for a client work item.
#
# Usage: ./set_password.sh <work-name>
#
# The script prompts twice for the password without echoing it, then writes
# it to <photos>/work/<work-name>/.password (mode 600). The server reads this
# file at request time, so no rebuild or restart is needed.

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <work-name>" >&2
    exit 2
fi

name="$1"

if [[ -z "$name" || "$name" == .* || "$name" == */* || "$name" == *\\* ]]; then
    echo "error: invalid work name: $name" >&2
    exit 2
fi

# Find photos/ as a sibling of the portfolio-site checkout containing this script.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
photos_dir="$(cd -- "$script_dir/.." && pwd)/photos"
work_dir="$photos_dir/work/$name"

if [[ ! -d "$work_dir" ]]; then
    echo "error: work folder not found: $work_dir" >&2
    exit 1
fi

read -r -s -p "Password for '$name': " pw1
echo
read -r -s -p "Confirm password: " pw2
echo

if [[ "$pw1" != "$pw2" ]]; then
    echo "error: passwords did not match" >&2
    exit 1
fi

# Mirror the server's trim: leading/trailing whitespace is stripped.
trimmed="$(printf '%s' "$pw1" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
if [[ -z "$trimmed" ]]; then
    echo "error: password is empty after trimming" >&2
    exit 1
fi

pw_file="$work_dir/.password"
umask 077
printf '%s\n' "$trimmed" > "$pw_file"
chmod 600 "$pw_file"

echo "wrote $pw_file"

# Offer to prebuild the download archives so the client never waits on the
# first download click. Only proposes it when the release binary exists.
bin="$script_dir/target/release/portfolio-site"
if [[ -x "$bin" ]]; then
    read -r -p "Prebuild zip archives for '$name' now? (y/N) " yn
    if [[ "$yn" =~ ^[Yy] ]]; then
        (cd "$script_dir" && "$bin" prebuild "$name")
    fi
fi
