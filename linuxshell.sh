#!/bin/bash

# before running the script, please configure the value


# on DHCP server 
sudo systemctl disable systemd-resolved
sudo systemctl stop systemd-resolved
sudo systemctl daemon-reload

sudo apt update -y
sudo apt install dnsmasq -y

sudo mv /etc/dnsmasq.conf /etc/dnsmasq.conf.bak
#sudo nano /etc/dnsmasq.conf
cat <<EOF>/etc/dnsmasq.conf
interface=eth0
dhcp-range=192.168.5.100,192.168.5.200,255.255.255.0,24h
dhcp-option=3,192.168.5.1
dhcp-option=6,192.168.5.1
EOF

sudo systemctl restart dnsmasq
sudo systemctl enable dnsmasq


# on DHCP client

# sudo nano /etc/netplan/01-netcfg.yaml
# network:
# version: 2
# renderer: networkd
# ethernets:
#   enp0s3:
#     dhcp4: true

# sudo netplan apply
