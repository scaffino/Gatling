# Install cargo and alto dependencies
curl https://sh.rustup.rs -sSf | sh
. "$HOME/.cargo/env"
sudo apt install -y pkg-config libssl-dev build-essential

apt install rustup
rustup toolchain install nightly

# Install python3 and 
sudo apt install -y python3 python3-pip python3-venv

# Clone alto
git clone https://my-alto-folder.git
cd alto
git fetch 
git checkout remote-deployments
cargo build --release --bin validator
