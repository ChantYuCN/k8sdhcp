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

ip link add 192.168.5.1/24 dev eth0
sudo systemctl restart dnsmasq
sudo systemctl enable dnsmasq


# on DHCP client

# sudo nano /etc/netplan/01-netcfg.yaml
# network:
#   version: 2
#   ethernets:
#     enp0s3:
#       dhcp4: true

# sudo netplan apply


#sudo apt install tftp-hpa  tftpd-hpa -y
sudo apt install dnsmasq -y

cat <<EOF>/etc/dnsmasq.conf
enable-tftp
tftp-root=/var/lib/tftpboot/
#Optional: Secure mode (only serve files owned by dnsmasq user)
#tftp-secure
#dhcp-boot=pxelinux.0,pxeserver,192.168.6.100
dhcp-boot=bootx64.efi
pxe-prompt="Press SPACE to boot from network or 'e' to exit",3
pxe-service=x86PC,"Boot from network",pxelinux
EOF

wget https://old-releases.ubuntu.com/releases/plucky/ubuntu-25.04-netboot-amd64.tar.gz
sudo mkdir -p /var/lib/tftpboot/
tar -zxvf ubuntu-25.04-netboot-amd64.tar.gz 
cp -r amd64/* /var/lib/tftpboot/
sudo sed -i "s#https://releases.ubuntu.com/25.04#http://192.168.6.1/iso#g" /var/lib/tftpboot/grub/grub.cfg
sudo chmod -R 777  /var/lib/tftpboot/


sudo systemctl restart dnsmasq
#sudo systemctl restart tftp-hpa

sudo apt install apache2 -y
sudo mkdir -p /var/www/html/iso

wget https://old-releases.ubuntu.com/releases/plucky/ubuntu-25.04-beta-live-server-amd64.iso 
sudo cp ubuntu-25.04-beta-live-server-amd64.iso  /var/www/html/iso
sudo chmod -R 777 /var/www/html/iso

cat /etc/apache2/sites-enabled/000-default.conf
...
   DocumentRoot /var/www/html/iso
...

sudo systemctl daemon-reload
sudo systemctl restart apache2

## Lite PXE server is ready
