FROM rust:1-bookworm

# eframe/egui Linux build deps, plus libdbus (keyring), cmake/perl (vendored SQLCipher/OpenSSL).
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libclang-dev libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev \
    libxcb-xfixes0-dev libxkbcommon-dev libssl-dev libwayland-dev libgl1-mesa-dev \
    libdbus-1-dev cmake perl \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt

WORKDIR /app
