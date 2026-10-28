Step-by-step: build a bootable live ISO from /mnt
Assumes /dev/sda1 is the root partition of the installed Ubuntu, already mounted at /mnt. Run everything on a Linux machine as root (sudo -i). You need free disk space of at least the size of the used root filesystem, plus the size of the ISO.

Step 1: Install the tools

apt update
apt install -y squashfs-tools xorriso grub-pc-bin grub-efi-amd64-bin grub-efi-amd64-signed shim-signed mtools dosfstools
Step 2: Prepare the chroot (make the system live-capable)
Mount the virtual filesystems into the installed system:


mount --bind /dev  /mnt/dev
mount --bind /proc /mnt/proc
mount --bind /sys  /mnt/sys
mount --bind /run  /mnt/run
cp /etc/resolv.conf /mnt/etc/resolv.conf   # network inside chroot
If /boot or /boot/efi is a separate partition, mount it first (for example mount /dev/sda2 /mnt/boot).

Enter the chroot:


chroot /mnt /bin/bash
Inside the chroot, install the live-boot components and rebuild the initrd:


apt update
apt install -y casper
# Make initramfs include NFS/network support (useful for PXE)
sed -i 's/^BOOT=.*/BOOT=nfs/' /etc/initramfs-tools/initramfs.conf 2>/dev/null
update-initramfs -u -k all

# Clean machine-specific data
truncate -s 0 /etc/machine-id
rm -f /etc/ssh/ssh_host_*
apt clean
rm -rf /tmp/* /var/tmp/*
history -c
exit
Note: the BOOT=nfs line is only for NFS-root PXE. For the HTTP ISO method, leave initramfs.conf unchanged. Casper handles it.

Unmount the bind mounts:


umount /mnt/dev /mnt/proc /mnt/sys /mnt/run
Step 3: Create the ISO directory layout

mkdir -p ~/iso/casper ~/iso/boot/grub
Step 4: Copy kernel and initrd

ls /mnt/boot/                      # check the versions
cp /mnt/boot/vmlinuz   ~/iso/casper/vmlinuz   # symlink to the latest kernel
cp /mnt/boot/initrd.img ~/iso/casper/initrd
If those symlinks don't exist, use the full names, for example vmlinuz-6.8.0-xx-generic.

Step 5: Compress the root filesystem

mksquashfs /mnt ~/iso/casper/filesystem.squashfs -comp xz -b 1M \
  -e boot/efi/* proc/* sys/* dev/* run/* tmp/* mnt/* media/* lost+found swapfile swap.img
Run this from a location outside /mnt. It can take a long time. Check the result:


ls -lh ~/iso/casper/
Step 6: Write the GRUB config

cat > ~/iso/boot/grub/grub.cfg <<'EOF'
set default=0
set timeout=5

menuentry "Ubuntu Live (from installed system)" {
    linux  /casper/vmlinuz boot=casper quiet splash ---
    initrd /casper/initrd
}
menuentry "Ubuntu Live (safe graphics)" {
    linux  /casper/vmlinuz boot=casper nomodeset ---
    initrd /casper/initrd
}
EOF
Step 7: Build the ISO

grub-mkrescue -o ~/ubuntu-live.iso ~/iso -- -volid UBUNTU_LIVE
This produces a hybrid ISO that boots on both BIOS and UEFI.

Step 8: Test before using PXE

apt install -y qemu-system-x86
qemu-system-x86_64 -m 4096 -cdrom ~/ubuntu-live.iso -boot d
You can also write it to a USB stick with dd if=~/ubuntu-live.iso of=/dev/sdX bs=4M status=progress (double check sdX, as it gets overwritten).

Step 9: Use it for PXE
Copy these files to your PXE/HTTP server:

~/iso/casper/vmlinuz and ~/iso/casper/initrd go in the TFTP root.
~/ubuntu-live.iso goes on the HTTP server.
Example pxelinux.cfg/default:


DEFAULT live
LABEL live
  KERNEL vmlinuz
  INITRD initrd
  APPEND root=/dev/ram0 ramdisk_size=3500000 ip=dhcp url=http://SERVER/ubuntu-live.iso boot=casper --
