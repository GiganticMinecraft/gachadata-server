# syntax=docker/dockerfile:1
# NOTE: Rustのバージョンはrust-toolchain.tomlと合わせること (Renovateはこのタグ形式を追跡できない)。
# また、実行ステージ (ubuntu:24.04, glibc 2.39) で動くバイナリにするため、
# glibcがより古いbookworm variantを使う (デフォルトのtrixieはglibc 2.41)
FROM lukemathwalker/cargo-chef:latest-rust-1.97.1-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY --link . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build-env
COPY --from=planner --link /app/recipe.json recipe.json

# Build dependencies - this is the caching Docker layer!
RUN cargo chef cook --release --recipe-path recipe.json

# Build application
COPY --link . .
RUN cargo build --release

FROM ubuntu:24.04
LABEL org.opencontainers.image.source=https://github.com/GiganticMinecraft/gachadata-server
RUN apt-get update -y && apt-get install -y curl

RUN curl -LsSO https://downloads.mariadb.com/MariaDB/mariadb_repo_setup
RUN echo "b54c87edfe81b9837ef44a4a4f39383dd8df32776e6a18c0743a5d3ece044ac3 mariadb_repo_setup" \
        | sha256sum -c -
RUN chmod +x mariadb_repo_setup
RUN ./mariadb_repo_setup \
       --mariadb-server-version="mariadb-11.4.7"
RUN apt update -y && apt install -y mariadb-client

COPY --from=build-env --link /app/target/release/gachadata-server /
CMD ["./gachadata-server"]
