# ===== FIRST STAGE ======

FROM rust:1.95-bookworm AS builder
LABEL description="This is the build stage for the wallet. Here we create the binary."

RUN apt-get update && apt-get install -y pkg-config && rm -rf /var/lib/apt/lists/*

WORKDIR /wallet

# We are copying only the files we need for building
# As any changes in other files will make the multi-stage build useless
COPY src src
COPY graphql graphql
COPY Cargo.lock Cargo.lock
COPY Cargo.toml Cargo.toml

RUN cargo build --release

# ===== SECOND STAGE ======

FROM debian:bookworm-slim AS runner
LABEL description="This is the 2nd stage: a very small image where we copy the wallet binary."

# rustls-based reqwest needs CA certificates.
RUN apt-get update && \
    apt-get install -y ca-certificates && \
    update-ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# ===== THIRD STAGE ======

FROM runner

WORKDIR /wallet

COPY --chmod=0755 ./scripts/start.sh /usr/local/bin
COPY --chmod=0755 --from=builder /wallet/target/release/wallet-daemon /usr/local/bin/wallet

RUN mkdir -p /wallet/store && \
    chmod 0700 /wallet/store

CMD ["/usr/local/bin/start.sh"]
