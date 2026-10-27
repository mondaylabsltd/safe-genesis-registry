# The docker shell (genesis-server): one container, one volume.
#   docker compose up -d            # with a .env next to docker-compose.yaml
FROM rust:1.97-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY genesis-rules genesis-rules
COPY genesis-server genesis-server
RUN cargo build --release --locked -p genesis-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir /data && chown 65534:65534 /data
COPY --from=build /src/target/release/genesis-server /usr/local/bin/genesis-server
ENV DATABASE_PATH=/data/genesis.sqlite LISTEN_ADDR=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
USER 65534:65534
ENTRYPOINT ["genesis-server"]
