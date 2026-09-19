# syntax=docker/dockerfile:1

# Build: the toolchain matches rust-toolchain.toml.
FROM rust:1.98.1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p tpf3mp-server \
    && cp target/release/tpf3mp-server /tpf3mp-server \
    && strip /tpf3mp-server \
    && mkdir -p /data

# Run: no shell, no package manager, a non-root user (UID 65532).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tpf3mp-server /usr/local/bin/tpf3mp-server
# A named volume mounted here starts with this owner, so the server can
# create its invite key without root.
COPY --from=build --chown=65532:65532 /data /data
USER 65532:65532
EXPOSE 29470/udp 29471/tcp 9470/tcp
ENTRYPOINT ["/usr/local/bin/tpf3mp-server"]
CMD ["--listen", "0.0.0.0:29470", \
     "--cert", "/certs/fullchain.pem", "--key", "/certs/privkey.pem", \
     "--secret-file", "/data/invite.key", \
     "--data-dir", "/data/rooms", \
     "--admin-listen", "0.0.0.0:9470", \
     "--tunnel-listen", "0.0.0.0:29471", "--tunnel-behind-proxy"]
