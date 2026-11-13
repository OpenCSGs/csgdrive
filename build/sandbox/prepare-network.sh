#!/usr/bin/env bash
set -euo pipefail
owner_uid=${1:?usage: sudo prepare-network.sh AGENTDRIVE_UID}
[[ $EUID == 0 && $owner_uid =~ ^[0-9]+$ ]] || { echo 'Root and a numeric UID are required' >&2; exit 1; }
# Dedicated interfaces and rules; existing host routes and firewall chains are preserved.
for slot in 0 1; do
    tap="adsb${slot}"
    gateway=$((slot * 4 + 1))
    if ! ip link show "$tap" >/dev/null 2>&1; then
        ip tuntap add dev "$tap" mode tap user "$owner_uid"
    fi
    ip address replace "172.30.80.${gateway}/30" dev "$tap"
    ip link set "$tap" up
done
sysctl -w net.ipv4.ip_forward=1
iptables -w -N ADSB-OUT 2>/dev/null || true
iptables -w -N ADSB-IN 2>/dev/null || true
add_rule() { local chain=$1; shift; iptables -w -C "$chain" "$@" 2>/dev/null || iptables -w -A "$chain" "$@"; }
for cidr in 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 127.0.0.0/8; do
    add_rule ADSB-OUT -d "$cidr" -j REJECT
done
for slot in 0 1; do
    guest=$((slot * 4 + 2))
    add_rule ADSB-OUT -i "adsb${slot}" -s "172.30.80.${guest}/32" -j ACCEPT
    add_rule ADSB-IN -o "adsb${slot}" -d "172.30.80.${guest}/32" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
done
add_rule ADSB-OUT -j DROP
add_rule ADSB-IN -j DROP
iptables -w -C FORWARD -i 'adsb+' -j ADSB-OUT 2>/dev/null || iptables -w -I FORWARD 1 -i 'adsb+' -j ADSB-OUT
iptables -w -C FORWARD -o 'adsb+' -j ADSB-IN 2>/dev/null || iptables -w -I FORWARD 1 -o 'adsb+' -j ADSB-IN
iptables -w -C INPUT -i 'adsb+' -j DROP 2>/dev/null || iptables -w -I INPUT 1 -i 'adsb+' -j DROP
iptables -w -t nat -C POSTROUTING -s 172.30.80.0/29 ! -o 'adsb+' -j MASQUERADE 2>/dev/null || iptables -w -t nat -A POSTROUTING -s 172.30.80.0/29 ! -o 'adsb+' -j MASQUERADE
# This configuration provides IPv4 Internet access only.
for chain in INPUT FORWARD; do
    ip6tables -w -C "$chain" -i 'adsb+' -j DROP 2>/dev/null || ip6tables -w -I "$chain" 1 -i 'adsb+' -j DROP
done
ip6tables -w -C FORWARD -o 'adsb+' -j DROP 2>/dev/null || ip6tables -w -I FORWARD 1 -o 'adsb+' -j DROP
