FROM rust:slim-trixie as builder
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential \
        meson \
        ninja-build \
        pkg-config \
        python3 \
        nasm \
        cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /usr/src/app
COPY . .
RUN cargo install --path .

FROM debian:trixie-slim
COPY --from=builder /usr/local/cargo/bin/comiconv-web /usr/local/bin/comiconv-web
CMD ["comiconv-web"]
