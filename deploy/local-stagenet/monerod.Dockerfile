# Match the wallet-rpc version the TEE container ships (see ../Dockerfile).
# Without an exact version match, regtest fork-version reporting drifts
# and wallet-rpc rejects the daemon with "Unexpected hard fork version".
FROM debian:bookworm-slim

ARG MONERO_VERSION=0.18.5.0
ARG MONERO_SHA256=166ad93036f95f5abeba24c8670061be022c9238dba2e6a7587611a1d759e294

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl ca-certificates bzip2 tini \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /tmp
RUN curl -fsSL -o monero.tar.bz2 \
        "https://downloads.getmonero.org/cli/monero-linux-x64-v${MONERO_VERSION}.tar.bz2" \
 && echo "${MONERO_SHA256}  monero.tar.bz2" | sha256sum -c - \
 && tar -xjf monero.tar.bz2 \
 && mv monero-x86_64-linux-gnu-v${MONERO_VERSION}/monerod /usr/local/bin/ \
 && rm -rf monero.tar.bz2 monero-x86_64-linux-gnu-v${MONERO_VERSION}

# monerod refuses to run as root by default; use the unprivileged user
# the upstream sethsimmons image also created.
RUN useradd -r -u 1000 monero && mkdir -p /home/monero/.bitmonero \
 && chown -R monero:monero /home/monero
USER monero
WORKDIR /home/monero

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/monerod"]
