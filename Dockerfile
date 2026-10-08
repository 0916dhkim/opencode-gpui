FROM rust:bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        cmake \
        fonts-noto-core \
        libdbus-1-dev \
        libexpat1-dev \
        libfontconfig1-dev \
        libfreetype6-dev \
        libgl1-mesa-dev \
        libssl-dev \
        libwebkit2gtk-4.1-dev \
        libxkbcommon-dev \
        libxkbcommon-x11-dev \
        libwayland-dev \
        libx11-xcb-dev \
        libzstd-dev \
        libvulkan1 \
        mesa-vulkan-drivers \
        openbox \
        picom \
        vulkan-tools \
        weston \
        xvfb \
        xdotool \
        x11-utils \
        imagemagick \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install stable && rustup default stable && rustup component add clippy rustfmt

WORKDIR /app
COPY . .
CMD ["cargo", "test", "--all-targets", "--locked"]
