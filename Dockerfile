FROM rust:1.94-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY build_support ./build_support
COPY data ./data
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --uid 10001 --create-home sparkrouter \
    && mkdir /data && chown sparkrouter:sparkrouter /data
COPY --from=build /build/target/release/sparkrouter /usr/local/bin/sparkrouter
COPY LICENSE THIRD_PARTY_NOTICES.md /usr/share/doc/sparkrouter/
USER sparkrouter
WORKDIR /data
ENV ROUTER_BIND=0.0.0.0:8080 ROUTER_DATABASE_URL=sqlite:///data/router.db RUST_LOG=info
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["sparkrouter"]
