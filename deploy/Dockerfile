# The control plane's image: one binary (glibc, SQLite compiled in) on distroless, running as uid 65532.
# Build from the repository root: docker build -f deploy/Dockerfile -t fleetlayer-control .

FROM rust:1.99.1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=fleetlayer-control-target,target=/src/target \
    cargo build --release --locked -p fleetlayer-control \
    && cp target/release/fleetlayer-control /fleetlayer-control

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /fleetlayer-control /usr/local/bin/fleetlayer-control
USER 65532:65532
ENV FLEETLAYER_CONTROL_DB=/data/control.db \
    FLEETLAYER_CONTROL_LISTEN=0.0.0.0:7600 \
    FLEETLAYER_CONTROL_HOOKS_LISTEN=0.0.0.0:7601
VOLUME /data
# The API and node links (private), and webhooks (the only port to expose publicly).
EXPOSE 7600 7601
ENTRYPOINT ["/usr/local/bin/fleetlayer-control"]
CMD ["serve"]
