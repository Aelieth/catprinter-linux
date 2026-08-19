#!/bin/sh
# Fleet provisioning for the Fedora/RHEL family (dnf). Installs the base packages a fleet machine has,
# sets the fleet-like unit state (that distro's unit names), and writes a fixed machine-id so the boot
# is the REBASE case (machine-id present => not a first boot => presets are NOT applied).
set -eu

# ghostscript comes with cups-filters (gstoraster); systemd + dbus-broker make it bootable.
dnf -y install --setopt=install_weak_deps=False \
    systemd dbus-broker dbus-common cups cups-client cups-filters bluez avahi avahi-tools \
    util-linux policycoreutils curl iproute procps-ng
dnf clean all

# cupsd socket-activated (cups.service disabled — catprinter-queue.service Wants= it anyway), avahi on,
# no interactive first-boot wizard.
systemctl enable cups.socket cups.path dbus-broker.service avahi-daemon.service
systemctl disable cups.service
systemctl mask systemd-firstboot.service

echo 8f5f8b7c2c8e4a7d9e2b1c3d4e5f6a7b > /etc/machine-id
