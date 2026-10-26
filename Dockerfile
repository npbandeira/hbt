# Imagem com hbt-server e hbt (cliente). Usada pelo teste de carga em
# docker/compose.loadtest.yml, mas serve para rodar o servidor em produção também.
#   docker build -t hbt .
#   docker run --rm -p 7000:7000 -p 8080:8080 -e HBT_DOMAIN=localhost hbt
#   docker build --target bench -t hbt:bench .   # + hbt-udpbench (teste de carga)
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p hbt-server -p hbt -p hbt-bench \
 && cp target/release/hbt-server target/release/hbt target/release/hbt-udpbench /usr/local/bin/

FROM debian:bookworm-slim AS runtime
COPY --from=build /usr/local/bin/hbt-server /usr/local/bin/hbt /usr/local/bin/
ENV HBT_DATA_DIR=/data HBT_HTTP_BIND=0.0.0.0:8080
VOLUME /data
EXPOSE 7000 8080
CMD ["hbt-server", "serve"]

FROM runtime AS bench
COPY --from=build /usr/local/bin/hbt-udpbench /usr/local/bin/

# Último estágio = alvo padrão do `docker build`.
FROM runtime
