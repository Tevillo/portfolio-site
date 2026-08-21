#!/bin/sh
# Abort on the first failure. Without this a failed `cargo build` still fell
# through to `systemctl restart`, relaunching the *old* binary as if the deploy
# had succeeded.
set -eu

git pull
cargo build --release
# Pre-generate every rendition so the first visitor after a deploy
# never pays the on-demand decode cost. Safe to run against the live server
# (atomic writes); already-fresh renditions are skipped cheaply.
./target/release/portfolio-site warm
sudo systemctl restart portfolio-site.service
