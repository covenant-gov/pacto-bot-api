# syntax=docker/dockerfile:1

# Multi-stage build for the Pacto bot API daemon and admin CLI.
#
# The image is intended for Docker Compose deployments. The daemon is the
# default command; the admin CLI is available at /usr/local/bin/pacto-bot-admin.
#
# Example usage:
#   docker build -t pacto-bot-api .
#   docker run -v $(pwd)/pacto-bot-api.toml:/etc/pacto/pacto-bot-api.toml:ro \
#     -v pacto-data:/var/lib/pacto-bot-api pacto-bot-api

FROM rust:1.96 AS builder

WORKDIR /usr/src/pacto-bot-api

COPY Cargo.toml Cargo.lock ./
COPY xtask xtask
COPY schemas schemas
COPY src src

ARG GIT_COMMIT_SHORT=unknown
ENV GIT_COMMIT_SHORT=${GIT_COMMIT_SHORT}

RUN cargo build --release --bins

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 1000 pacto \
    && useradd --system --uid 1000 --gid pacto \
       --home-dir /var/lib/pacto-bot-api --shell /sbin/nologin pacto

RUN mkdir -p /etc/pacto /var/lib/pacto-bot-api \
    && chown -R pacto:pacto /etc/pacto /var/lib/pacto-bot-api

COPY --from=builder /usr/src/pacto-bot-api/target/release/pacto-bot-api /usr/local/bin/
COPY --from=builder /usr/src/pacto-bot-api/target/release/pacto-bot-admin /usr/local/bin/

USER pacto
WORKDIR /var/lib/pacto-bot-api

VOLUME ["/var/lib/pacto-bot-api"]

# Optional localhost HTTP transport. The daemon only binds to it when started
# with --enable-http.
EXPOSE 9800

CMD ["pacto-bot-api", "--config", "/etc/pacto/pacto-bot-api.toml", "--data-dir", "/var/lib/pacto-bot-api"]
