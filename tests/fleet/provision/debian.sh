#!/bin/sh
# Fleet provisioning for the Debian/Ubuntu family (apt). systemd 252 on Debian 12 is also the runtime
# proof that catprinter.service's RestartSteps/RestartMaxDelaySec (systemd v254+) degrade gracefully.
set -eu
export DEBIAN_FRONTEND=noninteractive

apt-get update
# systemd-sysv provides /sbin/init; cups-ipp-utils provides driverless/ipptool; mawk/findutils are
# used by install.sh (awk, and `find` on the --download path); ca-certificates for curl over HTTPS.
apt-get install -y --no-install-recommends \
    systemd systemd-sysv dbus cups cups-client cups-filters cups-ipp-utils bluez \
    avahi-daemon avahi-utils rfkill util-linux curl iproute2 procps \
    mawk findutils ca-certificates
apt-get clean
rm -rf /var/lib/apt/lists/*

# cupsd is socket-activated on Debian; catprinter-queue.service Wants=cups.service anyway. dbus.service
# (not dbus-broker) is the Debian default. Guard each: unit names vary and a missing one is not fatal.
systemctl enable cups.socket avahi-daemon.service dbus.service 2>/dev/null || true
systemctl disable cups.service 2>/dev/null || true
systemctl mask systemd-firstboot.service 2>/dev/null || true

echo 8f5f8b7c2c8e4a7d9e2b1c3d4e5f6a7b > /etc/machine-id
