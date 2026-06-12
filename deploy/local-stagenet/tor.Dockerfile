FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        tor tini \
    && rm -rf /var/lib/apt/lists/* \
 && rm -f /etc/tor/torrc

# We need /var/lib/tor writable by the tor process. Debian's tor package
# creates user `debian-tor` (uid varies); make the dir ahead of time and
# let the entrypoint chown after the volume mount happens.
RUN mkdir -p /var/lib/tor && chown debian-tor:debian-tor /var/lib/tor

COPY tor-entrypoint.sh /usr/local/bin/tor-entrypoint.sh
RUN chmod +x /usr/local/bin/tor-entrypoint.sh

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/tor-entrypoint.sh"]
