#!/bin/bash
# Terminates the instance on a timer first, so a failure below can't leave it running.
shutdown -h +${lifetime_minutes}

set -euxo pipefail
apt-get update
apt-get install -y --no-install-recommends openjdk-21-jre-headless procps iproute2 sysstat fio

# Local NVMe for the Archive, so disk sync is measured on a real disk.
disk=$(lsblk -dpno NAME,MODEL | awk '/Instance Storage/ {print $1; exit}')
mkfs.ext4 -F -E nodiscard "$disk"
mkdir -p /data
mount -o noatime "$disk" /data
chmod 777 /data

cat > /etc/sysctl.d/90-aeron.conf <<'SYSCTL'
net.core.rmem_max=8388608
net.core.wmem_max=8388608
vm.swappiness=0
SYSCTL
sysctl --system

mount -o remount,size=6G /dev/shm
touch /data/ready
