# Build a fully static musl binary. Alpine's Rust targets musl natively and links
# statically by default; bundled SQLite and ring only need the C toolchain.
FROM docker.io/library/rust:1.94-alpine AS build
RUN apk add --no-cache musl-dev file
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
COPY build_support ./build_support
COPY data ./data
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
RUN cargo build --locked --release \
    && file target/release/sparkrouter | grep -Eq 'static-pie linked|statically linked' \
    && mkdir -p /out/data

# distroless/static has no shell or package manager. CA certificates and tzdata
# are included, although Reqwest uses its embedded webpki roots.
FROM gcr.io/distroless/static-debian13
COPY --from=build /build/target/release/sparkrouter /usr/local/bin/sparkrouter
COPY --from=build --chown=10001:10001 /out/data /data
COPY LICENSE THIRD_PARTY_NOTICES.md /usr/share/doc/sparkrouter/
# Numeric UID/GID keeps existing volumes owned by 10001 writable.
USER 10001:10001
WORKDIR /data
ENV ROUTER_BIND=0.0.0.0:14673 ROUTER_DATABASE_URL=sqlite:///data/router.db RUST_LOG=info
EXPOSE 14673
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/sparkrouter"]
