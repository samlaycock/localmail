# syntax=docker/dockerfile:1

FROM rust:1.99-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY assets assets
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin localmail \
    && cp target/release/localmail /usr/local/bin/localmail
RUN mkdir /data

# Run: distroless as a non-root user. Captured messages (SQLite) live in /data.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /usr/local/bin/localmail /usr/local/bin/localmail
COPY --from=build --chown=nonroot:nonroot /data /data
ENV LOCALMAIL_HOST=0.0.0.0 \
    LOCALMAIL_DATA_DIR=/data
VOLUME /data
EXPOSE 9402 2525
ENTRYPOINT ["/usr/local/bin/localmail"]
