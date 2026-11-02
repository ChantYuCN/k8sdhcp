https://download-wistron-vendor.goskope.com/dlr/win64/iQXk9bqxQL8bn4dfJDBdazhjti2eerCg6ojsCij8


gta@D3XTester:~$ cat /etc/default/grub
GRUB_DEFAULT=""
GRUB_TIMEOUT_STYLE="menu"
GRUB_TIMEOUT="5"
GRUB_DISTRIBUTOR=`( . /etc/os-release; echo ${NAME:-Ubuntu} ) 2>/dev/null || echo Ubuntu`
GRUB_CMDLINE_LINUX_DEFAULT="systemd.log_target=null intel_iommu=off debug drm.debug=0xa ignore_loglevel pci=realloc=on panic_on_warn pcie_ports=native net.ifnames=0 biosdevname=0 fsck.mode=force fsck.repair=yes console=ttyS0,115200n8 console=tty1 xe.wedged_mode=2 modprobe.blacklist=i915,bochs,snd_hda_intel xe.force_probe=*"
GRUB_CMDLINE_LINUX=""
