#!/usr/bin/env bash
# Turns the fix job's runner into a sandbox for the agent. Run as root by
# `python3 -m autofix isolate`, after every setup step that needs the network:
#
#   isolate.sh <root> <user> <bun> <runner-home> <host>=<ipv4>...
#
# 1. <user>: unprivileged, no sudo, not in the docker group.
# 2. It owns the repos and CARGO_HOME under <root>; the Rust toolchain is
#    root-owned and read-only; the tools stay the runner's.
# 3. Its egress is firewalled (below). The runner user's network is untouched.
set -euo pipefail

root=$1 user=$2 bun=$3 runner_home=$4
shift 4

for pin in "$@"; do
  [[ $pin =~ ^[a-z0-9.-]+=[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || { echo "bad pin: $pin" >&2; exit 1; }
done

# --- The CLI's subprocess sandbox -----------------------------------------------
# CLAUDE_CODE_SUBPROCESS_ENV_SCRUB runs each command of the agent in bubblewrap
# (its own user and PID namespaces, the token variables unset). Ubuntu 24.04's
# AppArmor blocks unprivileged user namespaces; the runner is thrown away
# after the job. check-isolation proves the sandbox starts as the user.
if ! command -v bwrap >/dev/null; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get -qq -o Acquire::Retries=3 update
  apt-get -qq -o Acquire::Retries=3 install -y --no-install-recommends bubblewrap
fi
if [[ -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]]; then
  sysctl -q -w kernel.apparmor_restrict_unprivileged_userns=0
fi

# --- The user and its files ---------------------------------------------------
useradd --create-home --shell /bin/bash --user-group "$user"
# The runner's home holds the job's temp files, step scripts and outputs.
chmod 750 "$runner_home"

chown -R "$user:$user" "$root/work" "$root/cargo"
install -d -o "$user" -g "$user" -m 755 "$root/handoff"
chown -R root:root "$root/rustup"
chmod -R a+rX,go-w "$root/rustup"
install -d -m 755 "$root/bin"
install -m 755 "$bun" "$root/bin/bun"
ln -sf bun "$root/bin/bunx"

# --- Names ---------------------------------------------------------------------
# The user gets no DNS (below), so the hosts it may reach resolve from here.
for pin in "$@"; do
  printf '%s %s\n' "${pin#*=}" "${pin%%=*}" >> /etc/hosts
done

# --- Egress ----------------------------------------------------------------------
# Every packet the user sends goes through AUTOFIX_EGRESS, first in OUTPUT.
# Allowed: loopback (MySQL on 127.0.0.1:3307, servers the tests start) and
# HTTPS to the Anthropic API range and the pinned hosts. Everything else is
# rejected, DNS included: names resolve only through /etc/hosts, and nothing
# can tunnel out over DNS.
iptables -N AUTOFIX_EGRESS
iptables -A AUTOFIX_EGRESS -p udp --dport 53 -j REJECT
iptables -A AUTOFIX_EGRESS -p tcp --dport 53 -j REJECT
iptables -A AUTOFIX_EGRESS -d 127.0.0.53 -j REJECT
iptables -A AUTOFIX_EGRESS -o lo -j ACCEPT
# Docker may publish 127.0.0.1:3307 by DNAT to the container instead of
# through docker-proxy on loopback: match the address before translation.
iptables -A AUTOFIX_EGRESS -m conntrack --ctorigdst 127.0.0.0/8 -j ACCEPT
iptables -A AUTOFIX_EGRESS -p tcp --dport 443 -d 160.79.104.0/23 -j ACCEPT
for pin in "$@"; do
  iptables -A AUTOFIX_EGRESS -p tcp --dport 443 -d "${pin#*=}" -j ACCEPT
done
iptables -A AUTOFIX_EGRESS -j REJECT
iptables -I OUTPUT 1 -m owner --uid-owner "$user" -j AUTOFIX_EGRESS

ip6tables -N AUTOFIX_EGRESS
ip6tables -A AUTOFIX_EGRESS -p udp --dport 53 -j REJECT
ip6tables -A AUTOFIX_EGRESS -p tcp --dport 53 -j REJECT
ip6tables -A AUTOFIX_EGRESS -o lo -j ACCEPT
ip6tables -A AUTOFIX_EGRESS -p tcp --dport 443 -d 2607:6bc0::/48 -j ACCEPT
ip6tables -A AUTOFIX_EGRESS -j REJECT
ip6tables -I OUTPUT 1 -m owner --uid-owner "$user" -j AUTOFIX_EGRESS

echo "Isolated $user: egress to 160.79.104.0/23, 2607:6bc0::/48 and $# pinned address(es)."
