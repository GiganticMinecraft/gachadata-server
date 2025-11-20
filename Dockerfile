# syntax=docker/dockerfile:1.20
FROM lukemathwalker/cargo-chef:latest-rust-1.87 AS chef
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
RUN echo "c4a0f3dade02c51a6a28ca3609a13d7a0f8910cccbb90935a2f218454d3a914a mariadb_repo_setup" \
        | sha256sum -c -
RUN chmod +x mariadb_repo_setup
RUN ./mariadb_repo_setup \
       --mariadb-server-version="mariadb-11.4.7"
RUN apt update -y && apt install -y mariadb-client

COPY --from=build-env --link /app/target/release/gachadata-server /
CMD ["./gachadata-server"]
