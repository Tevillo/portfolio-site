git pull
cargo build --release
# Pre-generate grid + preview renditions so the first visitor after a deploy
# never pays the on-demand decode cost. Safe to run against the live server
# (atomic writes); already-fresh renditions are skipped cheaply.
./target/release/portfolio-site warm
sudo systemctl restart portfolio-site.service
