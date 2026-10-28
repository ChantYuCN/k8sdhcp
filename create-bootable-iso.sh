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
