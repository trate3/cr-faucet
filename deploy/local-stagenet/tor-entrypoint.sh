#!/usr/bin/env bash
# Make the bind-mounted volume usable by debian-tor (uid varies per host)
# and start tor as that user. Without the chown, tor refuses to write to
# /var/lib/tor when the bind-mount comes up empty + root-owned.
set -euo pipefail

chown -R debian-tor:debian-tor /var/lib/tor
exec runuser -u debian-tor -- tor -f /etc/tor/torrc
