#!/bin/sh
# Fleet provisioning for the Arch family (pacman). archlinux:latest is rolling, so the CI job that uses
# this runs informational (continue-on-error) — Fedora + Debian gate the release.
set -eu

pacman -Sy --needed --noconfirm \
    systemd cups cups-filters bluez bluez-utils avahi util-linux curl iproute2 procps-ng \
    gawk findutils sed grep coreutils tar diffutils
# Keep the cache out of the image layer.
pacman -Scc --noconfirm >/dev/null 2>&1 || true

# cupsd socket-activated; catprinter-queue.service Wants=cups.service anyway. dbus.service is provided
# by the dbus package (aliased even when dbus-broker is the implementation). Guard each: not fatal.
systemctl enable cups.socket avahi-daemon.service dbus.service 2>/dev/null || true
systemctl disable cups.service 2>/dev/null || true
systemctl mask systemd-firstboot.service 2>/dev/null || true

echo 8f5f8b7c2c8e4a7d9e2b1c3d4e5f6a7b > /etc/machine-id
