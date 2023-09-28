# syntax=docker/dockerfile:1.4
FROM lukemathwalker/cargo-chef:0.1.62-rust-1.72.0 AS chef
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

FROM ubuntu:22.04
LABEL org.opencontainers.image.source=https://github.com/GiganticMinecraft/gachadata-server
RUN apt-get update -y && apt-get install -y mysql-client
COPY --from=build-env --link /app/target/release/gachadata-server /
CMD ["./gachadata-server"]
