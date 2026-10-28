1. Mount the root partition

lsblk -f                          # find the root partition, e.g. /dev/sdb2
sudo mount /dev/sdb2 /mnt
# If /boot or /boot/efi are separate partitions, mount them too:
# sudo mount /dev/sdb1 /mnt/boot/efi
2. Simple option: a plain data ISO (not bootable)
This is fine for archiving the files.


sudo apt install xorriso
sudo xorriso -as mkisofs -r -J -joliet-long -V "UBUNTU_BACKUP" \
  -o ~/ubuntu-root.iso /mnt
Notes:

ISO9660 caps file size at 4 GB unless you use -iso-level 3 (xorriso handles this with Rock Ridge/UDF in most cases). Add -iso-level 3 if you get file-size errors.
The output ISO must not be written inside /mnt. Write it to another disk with enough space.
Exclude virtual and unneeded directories (proc, sys, dev, and so on) with -m:

sudo xorriso -as mkisofs -r -J -joliet-long -iso-level 3 -V "UBUNTU_BACKUP" \
  -m '/mnt/proc/*' -m '/mnt/sys/*' -m '/mnt/dev/*' -m '/mnt/run/*' \
  -m '/mnt/tmp/*' -m '/mnt/lost+found' \
  -o ~/ubuntu-root.iso /mnt
3. Bootable live ISO (the system runs from the ISO)
This takes more work. The easiest route is Cubic or Systemback-style tools. The manual way:


sudo apt install squashfs-tools xorriso grub-pc-bin grub-efi-amd64-bin mtools

mkdir -p ~/iso/{casper,boot/grub}

# 1. Compress the root filesystem
sudo mksquashfs /mnt ~/iso/casper/filesystem.squashfs -comp xz \
  -e proc sys dev run tmp mnt media lost+found swapfile

# 2. Copy kernel + initrd (versions will differ)
sudo cp /mnt/boot/vmlinuz-*  ~/iso/casper/vmlinuz
sudo cp /mnt/boot/initrd.img-* ~/iso/casper/initrd

# 3. GRUB config
cat > ~/iso/boot/grub/grub.cfg <<'EOF'
set default=0
set timeout=5
menuentry "Ubuntu Live" {
  linux /casper/vmlinuz boot=casper quiet splash ---
  initrd /casper/initrd
}
EOF

# 4. Build the hybrid BIOS/UEFI ISO
grub-mkrescue -o ~/ubuntu-live.iso ~/iso
Caveats:

The initrd must include casper (the live-boot scripts). An installed system's initrd usually doesn't, so install casper in a chroot of /mnt and run update-initramfs -u first. Otherwise, copy vmlinuz and initrd from the official Ubuntu ISO.
Remove or adjust /etc/fstab entries and any machine-specific config (SSH host keys, network config) for the live copy.
4. If you actually want a backup or clone
An ISO isn't ideal for backing up an installed system. Better options:

sudo tar --one-file-system -cpzf backup.tar.gz -C /mnt . for a file-level backup
sudo dd if=/dev/sdb of=disk.img bs=4M status=progress for a raw disk image
Clonezilla for a full disk image
