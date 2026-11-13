#!/bin/bash
set -eu
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /run /workspace
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /run
mount -t virtiofs workspace /workspace
ip link set lo up
address= gateway= dns=
for arg in $(cat /proc/cmdline); do
    case "$arg" in
        agentdrive.ip=*) address=${arg#*=} ;;
        agentdrive.gateway=*) gateway=${arg#*=} ;;
        agentdrive.dns=*) dns=${arg#*=} ;;
    esac
done
if [ -n "$address" ]; then
    interface=
    for attempt in {1..50}; do
        for device in /sys/class/net/*; do
            if [ "$(cat "$device/type")" = 1 ] && [ -e "$device/device" ]; then
                interface=${device##*/}
                break
            fi
        done
        [ -n "$interface" ] && break
        sleep 0.1
    done
    ip link set "$interface" up
    ip address replace "$address" dev "$interface"
    ip route replace default via "$gateway" dev "$interface"
    rm -f /etc/resolv.conf
    for server in ${dns//,/ }; do
        printf 'nameserver %s\n' "$server"
    done > /etc/resolv.conf
fi
export LANG=C.UTF-8
printf '127.0.0.1 localhost\n::1 localhost\n' > /etc/hosts
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
mkdir -p /home/user/Desktop
chown user:user /home/user/Desktop
if [ ! -e /home/user/Desktop/AgentDrive ] && [ ! -L /home/user/Desktop/AgentDrive ]; then
    ln -s /workspace /home/user/Desktop/AgentDrive
    chown -h user:user /home/user/Desktop/AgentDrive
fi
export DISPLAY=:0 XDG_SESSION_TYPE=x11 XDG_CURRENT_DESKTOP=ubuntu:GNOME
export GNOME_SHELL_SESSION_MODE=ubuntu LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
if [ -f /etc/agentdrive-runtime.json ]; then
    mkdir -p /agent-state
    mount -t virtiofs assistant-state /agent-state
    python3 /usr/local/bin/assistant-service.py &
fi
/usr/local/bin/start-desktop.sh &
python3 /usr/local/bin/guest-helper.py &
while true; do
    wait -n || true
    sleep 1
done
