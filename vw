https://download-wistron-vendor.goskope.com/dlr/win64/iQXk9bqxQL8bn4dfJDBdazhjti2eerCg6ojsCij8


gta@D3XTester:~$ cat /etc/default/grub
GRUB_DEFAULT=""
GRUB_TIMEOUT_STYLE="menu"
GRUB_TIMEOUT="5"
GRUB_DISTRIBUTOR=`( . /etc/os-release; echo ${NAME:-Ubuntu} ) 2>/dev/null || echo Ubuntu`
GRUB_CMDLINE_LINUX_DEFAULT="systemd.log_target=null intel_iommu=off debug drm.debug=0xa ignore_loglevel pci=realloc=on panic_on_warn pcie_ports=native net.ifnames=0 biosdevname=0 fsck.mode=force fsck.repair=yes console=ttyS0,115200n8 console=tty1 xe.wedged_mode=2 modprobe.blacklist=i915,bochs,snd_hda_intel xe.force_probe=*"
GRUB_CMDLINE_LINUX=""

C:\tstlog\intel\ONLINE\20261028\WTCI2643SD31000019N84588-301_RUNIN_WSDN005_20261028074045
C:\tstlog\intel\ONLINE\20261028\WTCI2643SD3100000EN84588-301_RUNIN_WSDN005_20261028025354
C:\tstlog\intel\ONLINE\20261027\WTCI2643SD31000011N84588-301_RUNIN_WSDN003_20261027042407

chant@oceanlighthouse:~$ lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS
NAME          SIZE TYPE FSTYPE   MOUNTPOINTS
loop0        66.8M loop squashfs /snap/core24/1587
loop1        14.2M loop squashfs /snap/desktop-security-center/206
loop2        16.5M loop squashfs /snap/firmware-updater/226
loop3           4K loop squashfs /snap/bare/5
loop4       273.7M loop squashfs /snap/firefox/8107
loop5        19.6M loop squashfs /snap/desktop-security-center/150
loop6       606.1M loop squashfs /snap/gnome-46-2404/153
loop7        91.7M loop squashfs /snap/gtk-common-themes/1535
loop8         395M loop squashfs /snap/mesa-2404/1165
loop9        18.8M loop squashfs /snap/prompting-client/204
loop10       18.4M loop squashfs /snap/prompting-client/228
loop11       15.7M loop squashfs /snap/snap-store/1367
loop12       49.3M loop squashfs /snap/snapd/26865
loop13        580K loop squashfs /snap/snapd-desktop-integration/361
sda         953.9G disk          
├─sda1        700G part ext4     
├─sda2        100M part vfat     
├─sda3          1G part ext3     
├─sda4        128M part ext3     
├─sda5          2G part vfat     /run/media/chant/GTAX_SHARED
└─sda6      228.3G part ntfs     /run/media/chant/CACHE
nvme0n1     931.5G disk          
├─nvme0n1p1     1G part vfat     /boot/efi
└─nvme0n1p2 930.5G part ext4     /

chant@oceanlighthouse:~$ sudo fdisk -l /dev/sda1 
Disk /dev/sda1: 700 GiB, 751619276800 bytes, 1468006400 sectors
Units: sectors of 1 * 512 = 512 bytes
Sector size (logical/physical): 512 bytes / 512 bytes
I/O size (minimum/optimal): 512 bytes / 33553920 bytes



