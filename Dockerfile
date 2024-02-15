FROM rust:1.76-buster as builder
LABEL description="This is the build stage for the wallet. Here we create the binary."

WORKDIR /wallet
COPY . .

RUN cargo build --release

# ===== SECOND STAGE ======

FROM debian:buster-slim
LABEL description="This is the 2nd stage: a very small image where we copy the wallet binary."
# reqwest needs libssl and curl is needed to install the ca-certificates
# awscli is needed for the start script to retrieve the secrets
RUN apt-get update && \
    apt-get install -y \
    libssl-dev \
    wait-for-it \
    jq \
    curl \
    zip && \
    rm -rf /var/lib/apt/lists && \
    curl "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" -o "/tmp/awscliv2.zip" && \
    unzip /tmp/awscliv2.zip -d /tmp/awscli && \
    /tmp/awscli/aws/install && \
    rm -rf /tmp/awscli*
RUN rm -rf /var/lib/apt/lists/*

COPY --from=builder /wallet/target/release/wallet /usr/local/bin
COPY --from=builder /wallet/scripts/start.sh /usr/local/bin

# STORE_NAME = the storage name, this is a hex number which is the file name where the key is stored,
# it's generated when the key is generate (In the current example: `73723235301cb3057d43941d5f631613aa1661be0354d39e34f23d4ef527396b10d2bb7a`)
# SEED_PHRASE = These are the content of the key file, which are the bip-39 words used to generate the key.
# (In the current example: "duty captain man fantasy angry window release hammer suspect bullet panda special")
# KEY_PASS = The pass of the key which when originally generated is set through the `KEY_PASS` env variable. (In the current example: `example`)
# PLATFORM_KEY = The platform key is the API token used to authenticate the wallet daemon so it can request new transactions from the platform to sign.
CMD ["start.sh"]
