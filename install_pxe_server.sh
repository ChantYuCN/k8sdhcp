#!/bin/bash
# Lite UEFI PXE server: dnsmasq (DHCP + DNS + TFTP) + Apache (HTTP for the ISO)
# Target OS: Ubuntu 25.04 live-server, installed over the network by UEFI clients.
#
# Usage: edit the CONFIG block, then run:  sudo ./pxeserver_installer.sh
set -euo pipefail

########################  CONFIG  ########################
IFACE="eth0"                      # NIC facing the target hosts
SERVER_IP="192.168.5.1"           # this server (also gateway/DNS/HTTP/TFTP)
PREFIX="24"
NETMASK="255.255.255.0"
DHCP_START="192.168.5.100"
DHCP_END="192.168.5.200"
LEASE="24h"
UPSTREAM_DNS="8.8.8.8"            # forwarded DNS for clients
SET_STATIC_IP=0                   # 1 = write+apply netplan for IFACE (don't use over SSH on IFACE!)

RELEASE_URL="https://old-releases.ubuntu.com/releases/plucky"
NETBOOT_TAR="ubuntu-25.04-netboot-amd64.tar.gz"
ISO_NAME="ubuntu-25.04-beta-live-server-amd64.iso"

TFTP_ROOT="/var/lib/tftpboot"
WEB_ROOT="/var/www/html"
ISO_DIR="${WEB_ROOT}/iso"
WORKDIR="$(mktemp -d)"
##########################################################

[[ $EUID -eq 0 ]] || { echo "Run as root (sudo)." >&2; exit 1; }
trap 'rm -rf "$WORKDIR"' EXIT

log() { echo -e "\n==> $*"; }

log "Installing packages"
apt-get update -y
apt-get install -y dnsmasq apache2 wget

# ---- 1. Server network (optional) ----
if [[ "$SET_STATIC_IP" == "1" ]]; then
  log "Configuring static IP ${SERVER_IP}/${PREFIX} on ${IFACE}"
  cat >/etc/netplan/60-pxe.yaml <<EOF
network:
  version: 2
  ethernets:
    ${IFACE}:
      dhcp4: false
      addresses: [${SERVER_IP}/${PREFIX}]
EOF
  chmod 600 /etc/netplan/60-pxe.yaml
  netplan apply
elif ! ip -4 addr show dev "$IFACE" | grep -q "inet ${SERVER_IP}/"; then
  echo "WARNING: ${IFACE} does not have ${SERVER_IP}. Set it manually or use SET_STATIC_IP=1." >&2
fi

# ---- 2. Free port 53 for dnsmasq (keep systemd-resolved running, just drop its stub) ----
if systemctl is-active --quiet systemd-resolved; then
  log "Disabling systemd-resolved stub listener"
  mkdir -p /etc/systemd/resolved.conf.d
  printf '[Resolve]\nDNSStubListener=no\n' >/etc/systemd/resolved.conf.d/nostub.conf
  ln -sf /run/systemd/resolve/resolv.conf /etc/resolv.conf
  systemctl restart systemd-resolved
fi

# ---- 3. dnsmasq: one config file for DHCP + DNS + TFTP + UEFI boot ----
log "Writing dnsmasq config"
[[ -f /etc/dnsmasq.conf && ! -f /etc/dnsmasq.conf.bak ]] && mv /etc/dnsmasq.conf /etc/dnsmasq.conf.bak
cat >/etc/dnsmasq.conf <<EOF
# --- listen only on the PXE NIC ---
interface=${IFACE}
bind-interfaces

# --- DHCP ---
dhcp-range=${DHCP_START},${DHCP_END},${NETMASK},${LEASE}
dhcp-option=option:router,${SERVER_IP}
dhcp-option=option:dns-server,${SERVER_IP}
dhcp-authoritative

# --- DNS (forward to upstream) ---
server=${UPSTREAM_DNS}
domain-needed
bogus-priv

# --- TFTP ---
enable-tftp
tftp-root=${TFTP_ROOT}

# --- UEFI PXE: match client arch (option 93): 7 = x64 UEFI, 9 = x64 UEFI (EBC) ---
dhcp-match=set:efi64,option:client-arch,7
dhcp-match=set:efi64,option:client-arch,9
dhcp-boot=tag:efi64,bootx64.efi,${SERVER_IP},${SERVER_IP}
# Non-UEFI (legacy BIOS) clients are intentionally not served.

log-dhcp
EOF
dnsmasq --test

# ---- 4. Netboot files (shim/grub/kernel/initrd) ----
log "Fetching netboot image"
mkdir -p "$TFTP_ROOT"
wget -nv -P "$WORKDIR" "${RELEASE_URL}/${NETBOOT_TAR}"
tar -xzf "${WORKDIR}/${NETBOOT_TAR}" -C "$WORKDIR"
cp -r "${WORKDIR}"/amd64/* "$TFTP_ROOT/"

# Point the installer at our HTTP server instead of the Internet
GRUB_CFG="${TFTP_ROOT}/grub/grub.cfg"
sed -i "s#https\?://releases.ubuntu.com/[^ ]*#http://${SERVER_IP}/iso#g" "$GRUB_CFG"
# the kernel needs the full ISO file name
sed -i "s#url=http://${SERVER_IP}/iso\([^/]\)#url=http://${SERVER_IP}/iso/${ISO_NAME}\1#g" "$GRUB_CFG"

# Read-only for everyone, owned by root (no 777)
chown -R root:root "$TFTP_ROOT"
find "$TFTP_ROOT" -type d -exec chmod 755 {} +
find "$TFTP_ROOT" -type f -exec chmod 644 {} +

# ---- 5. Apache serves the ISO at http://SERVER_IP/iso/ ----
log "Fetching ISO"
mkdir -p "$ISO_DIR"
[[ -f "${ISO_DIR}/${ISO_NAME}" ]] || wget -nv -c -O "${ISO_DIR}/${ISO_NAME}" "${RELEASE_URL}/${ISO_NAME}"
chmod 755 "$ISO_DIR"
chmod 644 "${ISO_DIR}/${ISO_NAME}"

# ---- 6. Start services ----
log "Starting services"
systemctl enable --now apache2
systemctl enable dnsmasq
systemctl restart dnsmasq

# ---- 7. Firewall (only if ufw is active) ----
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow in on "$IFACE" to any port 67 proto udp   # DHCP
  ufw allow in on "$IFACE" to any port 69 proto udp   # TFTP
  ufw allow in on "$IFACE" to any port 53             # DNS
  ufw allow in on "$IFACE" to any port 80 proto tcp   # HTTP
fi

# ---- 8. Sanity checks ----
log "Checks"
systemctl is-active dnsmasq apache2
curl -sfI "http://${SERVER_IP}/iso/${ISO_NAME}" | head -n1 || echo "WARNING: ISO not reachable over HTTP"
[[ -f "${TFTP_ROOT}/bootx64.efi" ]] || echo "WARNING: ${TFTP_ROOT}/bootx64.efi missing"

cat <<EOF

PXE server ready.
  DHCP/TFTP : ${SERVER_IP} (${DHCP_START}-${DHCP_END})
  HTTP ISO  : http://${SERVER_IP}/iso/${ISO_NAME}
Boot the target host in UEFI mode with "PXE IPv4" as first boot device.
Watch requests with: journalctl -fu dnsmasq
EOF
