#!/bin/bash
# Lite UEFI PXE server: dnsmasq (DHCP + DNS + TFTP) + Apache (HTTP for ISO / autoinstall / done-callback)
# Target OS: Ubuntu 25.04 live-server, installed unattended over the network by UEFI clients.
#
# Duplicate-install protection:
#   1. The autoinstall's last step calls http://SERVER/cgi-bin/pxe-done?mac=<MAC>
#      -> server creates  ${TFTP_ROOT}/done/<mac>
#   2. On every PXE boot grub.cfg checks for that marker. If present it exits grub,
#      so UEFI firmware falls through to the next boot entry (the installed disk).
#   A host whose install failed/aborted has no marker, so it retries automatically.
#   Re-install a host on purpose:  sudo pxe-ctl reinstall <mac>
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

# Fixed MAC -> IP bindings, "mac,ip[,hostname]". Use IPs outside the dynamic pool or inside it, both work.
# Manage later without re-running:  pxe-ctl bind|unbind|binds
HOSTS=(
  # "aa:bb:cc:dd:ee:01,192.168.5.11,node01"
  # "aa:bb:cc:dd:ee:02,192.168.5.12,node02"
)
ONLY_KNOWN_HOSTS=0                # 1 = only hosts listed in the bindings get DHCP/PXE (no dynamic leases)
HOSTS_FILE="/etc/dnsmasq.d/pxe-hosts"

# Autoinstall identity for target hosts (CHANGE the password!)
TARGET_HOSTNAME="ubuntu-node"
TARGET_USER="ubuntu"
TARGET_PASSWORD="ChangeMe123!"

RELEASE_URL="https://old-releases.ubuntu.com/releases/plucky"
NETBOOT_TAR="ubuntu-25.04-netboot-amd64.tar.gz"
ISO_NAME="ubuntu-25.04-beta-live-server-amd64.iso"

TFTP_ROOT="/var/lib/tftpboot"
DONE_DIR="${TFTP_ROOT}/done"      # one empty-ish file per installed MAC
WEB_ROOT="/var/www/html"
ISO_DIR="${WEB_ROOT}/iso"
AUTOINSTALL_DIR="${WEB_ROOT}/autoinstall"
WORKDIR="$(mktemp -d)"
##########################################################

[[ $EUID -eq 0 ]] || { echo "Run as root (sudo)." >&2; exit 1; }
trap 'rm -rf "$WORKDIR"' EXIT

log() { echo -e "\n==> $*"; }
[[ "$TARGET_PASSWORD" == "ChangeMe123!" ]] && echo "WARNING: using the default TARGET_PASSWORD, change it." >&2

log "Installing packages"
apt-get update -y
apt-get install -y dnsmasq apache2 wget openssl

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
mkdir -p "$(dirname "$HOSTS_FILE")"
touch "$HOSTS_FILE"
cat >/etc/dnsmasq.conf <<EOF
# --- listen only on the PXE NIC ---
interface=${IFACE}
bind-interfaces

# --- DHCP ---
dhcp-range=${DHCP_START},${DHCP_END},${NETMASK},${LEASE}
dhcp-option=option:router,${SERVER_IP}
dhcp-option=option:dns-server,${SERVER_IP}
dhcp-authoritative

# --- static MAC->IP bindings (re-read on SIGHUP, no restart needed) ---
dhcp-hostsfile=${HOSTS_FILE}
$([[ "$ONLY_KNOWN_HOSTS" == "1" ]] && echo "dhcp-ignore=tag:!known")

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

# Our own grub.cfg: skip hosts already installed, otherwise run the autoinstall.
# (\$ = grub variable, ${...} = filled in by this script; grub needs \; for a literal ;)
log "Writing grub.cfg (duplicate-install guard)"
mkdir -p "${TFTP_ROOT}/grub"
cat >"${TFTP_ROOT}/grub/grub.cfg" <<EOF
set timeout=5
set default=0

# Already installed? -> leave grub; UEFI firmware continues with the next boot entry (local disk).
if [ -f /done/\${net_default_mac} ]; then
    echo "Host \${net_default_mac} is already installed - booting from local disk."
    sleep 2
    exit
fi

menuentry "Autoinstall Ubuntu 25.04 (WIPES THE DISK)" {
    echo "Loading kernel..."
    linux /casper/vmlinuz ip=dhcp url=http://${SERVER_IP}/iso/${ISO_NAME} autoinstall "ds=nocloud-net\;s=http://${SERVER_IP}/autoinstall/" ---
    initrd /casper/initrd
}
menuentry "Boot from local disk" {
    exit
}
EOF

# ---- 5. "Installed" marker directory + admin helper ----
mkdir -p "$DONE_DIR"
# everything under TFTP_ROOT: root-owned, read-only (no 777) ...
chown -R root:root "$TFTP_ROOT"
find "$TFTP_ROOT" -type d -exec chmod 755 {} +
find "$TFTP_ROOT" -type f -exec chmod 644 {} +
# ... except the marker dir, which the Apache CGI (www-data) writes to
chown www-data:www-data "$DONE_DIR"

cat >/usr/local/sbin/pxe-ctl <<EOF
#!/bin/bash
# pxe-ctl list | reinstall <mac> | done <mac> | binds | bind <mac> <ip> [name] | unbind <mac|ip>
set -u
D="${DONE_DIR}"
H="${HOSTS_FILE}"
norm()  { echo "\$1" | tr 'A-F-' 'a-f:'; }
isMac() { [[ "\$1" =~ ^([0-9a-f]{2}:){5}[0-9a-f]{2}\$ ]]; }
isIp()  { [[ "\$1" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}\$ ]]; }
reload() { systemctl kill -s HUP dnsmasq 2>/dev/null || true; }
need_mac() { isMac "\$1" || { echo "invalid MAC: \$1" >&2; exit 1; }; }

case "\${1:-}" in
  list)      ls -l --time-style=long-iso "\$D" | tail -n +2 ;;
  reinstall) mac=\$(norm "\${2:-}"); need_mac "\$mac"; rm -fv "\$D/\$mac" ;;
  done)      mac=\$(norm "\${2:-}"); need_mac "\$mac"; date -Is > "\$D/\$mac"; chown www-data: "\$D/\$mac" ;;
  binds)     cat "\$H" ;;
  bind)      mac=\$(norm "\${2:-}"); ip="\${3:-}"; name="\${4:-}"
             need_mac "\$mac"; isIp "\$ip" || { echo "invalid IP: \$ip" >&2; exit 1; }
             # drop any old line using this MAC or this IP, then add the new binding
             grep -viE "^\$mac,|,\$ip(,|\$)" "\$H" > "\$H.tmp" || true
             echo "\$mac,\$ip\${name:+,\$name}" >> "\$H.tmp"; mv "\$H.tmp" "\$H"; reload
             echo "bound \$mac -> \$ip" ;;
  unbind)    key=\$(norm "\${2:-}"); [[ -n "\$key" ]] || exit 1
             grep -viE "^\$key,|,\$key(,|\$)" "\$H" > "\$H.tmp" || true
             mv "\$H.tmp" "\$H"; reload; echo "unbound \$key" ;;
  *)         echo "usage: pxe-ctl list | reinstall <mac> | done <mac> | binds | bind <mac> <ip> [name] | unbind <mac|ip>"; exit 1 ;;
esac
EOF
chmod 755 /usr/local/sbin/pxe-ctl

# Apply the HOSTS bindings from CONFIG (idempotent: replaces entries with the same MAC/IP)
for entry in "${HOSTS[@]}"; do
  IFS=, read -r h_mac h_ip h_name <<<"$entry"
  /usr/local/sbin/pxe-ctl bind "$h_mac" "$h_ip" "${h_name:-}"
done
dnsmasq --test

# ---- 6. Apache: ISO, autoinstall files, and the "done" callback CGI ----
log "Configuring Apache"
mkdir -p "$ISO_DIR" "$AUTOINSTALL_DIR"
[[ -f "${ISO_DIR}/${ISO_NAME}" ]] || wget -nv -c -O "${ISO_DIR}/${ISO_NAME}" "${RELEASE_URL}/${ISO_NAME}"
chmod 755 "$ISO_DIR" "$AUTOINSTALL_DIR"
chmod 644 "${ISO_DIR}/${ISO_NAME}"

PW_HASH="$(openssl passwd -6 "$TARGET_PASSWORD")"
: >"${AUTOINSTALL_DIR}/meta-data"
cat >"${AUTOINSTALL_DIR}/user-data" <<EOF
#cloud-config
autoinstall:
  version: 1
  locale: en_US.UTF-8
  keyboard: {layout: us}
  identity:
    hostname: ${TARGET_HOSTNAME}
    username: ${TARGET_USER}
    password: '${PW_HASH}'
  ssh:
    install-server: true
    allow-pw: true
  storage:
    layout:
      name: direct
  # Last step before reboot: tell the PXE server this MAC is done.
  late-commands:
    - |
      IF=\$(ip route show default | awk '{print \$5; exit}')
      MAC=\$(cat /sys/class/net/\$IF/address)
      curl -fsS --retry 5 --retry-delay 2 "http://${SERVER_IP}/cgi-bin/pxe-done?mac=\$MAC"
EOF
chmod 644 "${AUTOINSTALL_DIR}"/*

cat >/usr/lib/cgi-bin/pxe-done <<EOF
#!/bin/bash
mac=\$(echo "\$QUERY_STRING" | sed -n 's/^mac=\\([0-9A-Fa-f:]\\{17\\}\\)\$/\\1/p' | tr 'A-F' 'a-f')
if [[ -z "\$mac" ]]; then
  printf 'Status: 400 Bad Request\r\nContent-Type: text/plain\r\n\r\nbad mac\n'
  exit 0
fi
date -Is > "${DONE_DIR}/\$mac"
printf 'Content-Type: text/plain\r\n\r\nok %s\n' "\$mac"
EOF
chmod 755 /usr/lib/cgi-bin/pxe-done

# Only the PXE subnet may call the CGI
cat >/etc/apache2/conf-available/pxe-done.conf <<EOF
<Location "/cgi-bin/pxe-done">
    Require ip ${SERVER_IP%.*}.0/${PREFIX}
</Location>
EOF
a2enmod cgi >/dev/null
a2enconf serve-cgi-bin pxe-done >/dev/null

# ---- 7. Start services ----
log "Starting services"
systemctl enable apache2 dnsmasq
systemctl restart apache2 dnsmasq

# ---- 8. Firewall (only if ufw is active) ----
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow in on "$IFACE" to any port 67 proto udp   # DHCP
  ufw allow in on "$IFACE" to any port 69 proto udp   # TFTP
  ufw allow in on "$IFACE" to any port 53             # DNS
  ufw allow in on "$IFACE" to any port 80 proto tcp   # HTTP
fi

# ---- 9. Sanity checks ----
log "Checks"
systemctl is-active dnsmasq apache2
curl -sfI "http://${SERVER_IP}/iso/${ISO_NAME}" | head -n1 || echo "WARNING: ISO not reachable over HTTP"
curl -sf  "http://${SERVER_IP}/autoinstall/user-data" >/dev/null || echo "WARNING: autoinstall user-data not reachable"
curl -s   "http://${SERVER_IP}/cgi-bin/pxe-done?mac=bad" | grep -q "bad mac" || echo "WARNING: pxe-done CGI not working"
[[ -f "${TFTP_ROOT}/bootx64.efi" ]] || echo "WARNING: ${TFTP_ROOT}/bootx64.efi missing"

cat <<EOF

PXE server ready.
  DHCP/TFTP : ${SERVER_IP} (${DHCP_START}-${DHCP_END})
  HTTP ISO  : http://${SERVER_IP}/iso/${ISO_NAME}
Boot the target host in UEFI mode with "PXE IPv4" as first boot device.

Duplicate-install guard:
  Installed hosts are recorded in ${DONE_DIR}/<mac>
  sudo pxe-ctl list               # show installed hosts
  sudo pxe-ctl reinstall <mac>    # allow a host to be installed again
  sudo pxe-ctl done <mac>         # mark an existing host as installed (never PXE-install it)
Fixed IP bindings (${HOSTS_FILE}):
  sudo pxe-ctl binds                          # list
  sudo pxe-ctl bind <mac> <ip> [hostname]     # add/replace, applied immediately
  sudo pxe-ctl unbind <mac|ip>
Watch requests with: journalctl -fu dnsmasq
EOF
