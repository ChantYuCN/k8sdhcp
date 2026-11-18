# Estimating the time to deploy the OS image to many servers

This guide shows how to **measure** your own environment and **calculate** how long it takes to deploy the
Clonezilla image (`cri-golden`) to N servers at the same time with `pxe-installer`.

The PXE server sends the image to every target over **unicast NFS**: each target pulls its own full copy. So
the total traffic grows with the number of targets, and usually the **server** is the bottleneck, not the
targets.

> All numbers below are MB (megabytes) and MB/s unless stated. 1 Gbit/s is about 110 MB/s in practice.
> The result is an estimate (plus or minus 20-30 %). Always confirm it with a pilot of 10-20 servers (section 5).

---

## 1. The model

The restore is a pipeline: server disk -> server network -> target network -> decompress -> target disk.
The slowest stage sets the speed. Two limits matter:

| Limit | Meaning |
| --- | --- |
| **Server limit** | what the PXE server can send in total, shared by all N targets |
| **Target limit** | how fast one target restores when it has the server to itself |

```
server_limit = min(Net, Disk, Live)               # MB/s, measured in section 3
server_time  = N x I / server_limit               # seconds: time to push N copies out of the server
target_time  = T1                                 # seconds: restore time of ONE server alone (pilot)

deploy_time  = max(server_time, target_time) + overhead
```

- `N`  number of servers deployed at the same time
- `I`  image size on the server, in MB (compressed)
- `T1` restore time of one server deployed alone
- `overhead` boot, post-restore script and reboot, about 5-10 minutes (section 3.6)

If `server_time` is bigger than `target_time`, the **server** is the bottleneck. Otherwise the **targets** are.

---

## 2. Inputs you need

### 2.1 Image size `I` and used data `D`

On the PXE server:

```bash
du -sm /home/partimag/cri-golden            # I: size of the (compressed) image, in MB
```

`D` is the used data that is restored to each target (the source partitions' used space):

```bash
sudo lsblk -o NAME,SIZE,FSUSED,FSTYPE /dev/nvme0n2      # on the source machine, or: df -BM
```

`D / I` is the compression ratio (about 2 for typical OS data). `D` is only needed to report the speed as
"restored data per second"; the formula above only uses `I`.

### 2.2 Number of servers `N`

How many targets PXE boot at the same time. Note that the default DHCP range `192.168.5.100-200` has 101
addresses (enough for 100, but with no spare).

---

## 3. Measurements

Run each test and write the result into the worksheet in section 4.

### 3.1 Server network `Net` (many targets at once)

A single test flow overstates what 100 flows get (switch drops, TCP incast), so test with **10-20 targets
at the same time**.

Use the real path, NFS from the image folder. Boot the targets with the menu entry
**"Clonezilla Live (interactive ...)"** (it already mounts the image folder on `/home/partimag`), open a
shell and run the same command on all of them at the same moment:

```bash
# the image is split into files of about 4 GB (*.aa, *.ab ...): read them one after another, first 20 GB
cat /home/partimag/cri-golden/*ptcl-img* | dd of=/dev/null bs=1M count=20000     # prints MB/s at the end
```

Add up the MB/s of all targets: that is `Net`. (If `iperf3` is available, `iperf3 -s` on the server and
`iperf3 -c SERVER_IP -t 30` on the targets gives the pure network number; NFS may be slower than iperf3.)

> The very first reads can come from the server's RAM cache. To measure the disk path, drop the cache on the
> server first (3.2).

### 3.2 Server disk `Disk` (many parallel streams)

100 targets read at different positions of the same image, so the disk sees many interleaved streams. On the
PXE server, drop the cache and read with several streams at different offsets:

```bash
sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
# 16 streams, each reading a different image file (the biggest ones; needs at least 16 files in the image)
ls -S /home/partimag/cri-golden/*ptcl-img* | head -16 | while read f; do
  dd if="$f" of=/dev/null bs=1M &
done; wait
```

Add up the MB/s printed by the 16 `dd` commands: that is `Disk`. If the image has fewer than 16 files, use
`fio` (it can read one big test file with many streams):
`fio --name=r --rw=read --bs=1M --numjobs=16 --size=4G --directory=/home/partimag --group_reporting`
(it creates temporary files in that folder; delete them afterwards).
A hard-disk array drops sharply here; NVMe does not.

If the whole image fits in the server's RAM and the targets run roughly in step, the page cache serves most
reads and `Disk` can be ignored (use a large number).

### 3.3 Live load `Live` (during a pilot deploy)

While a pilot of k servers restores, watch the server. Record the **steady** outgoing rate, not a peak:

```bash
sar -n DEV 5                       # txkB/s of the PXE interface; MB/s = txkB/s / 1000
iostat -xm 5                       # disk: rMB/s, %util, r_await
cat /proc/net/rpc/nfsd | grep th   # nfsd threads (if all are busy, raise threads in /etc/nfs.conf [nfsd])
```

`Live` is the steady MB/s sent while k >= 10 servers were restoring. If it stays at about 90-95 % of the NIC
speed, the NIC is the bottleneck. If it is much lower but the targets are slow, look at disk `%util`, the
number of nfsd threads and the switch.

### 3.4 One server alone `T1`

Deploy **one** server with nothing else running and read its time:

```bash
sudo pxe-ctl times
```

`ELAPSED` is the time from the start of Clonezilla Live to the end of `post.sh`. That is `T1` (convert to
seconds). It already includes the target's network, decompression and disk write, so no separate target test
is needed.

### 3.5 Target disk write (optional check)

Only to explain a slow `T1`. **This erases the start of the disk: run it on a target that will be restored
anyway**, from the Clonezilla Live shell:

```bash
dd if=/dev/zero of=/dev/nvme0n1 bs=1M count=20000 oflag=direct status=progress
```

Compare the MB/s with `D / T1`. Decompression speed (optional):
`pzstd -dc /home/partimag/cri-golden/<disk>.ext4-ptcl-img.zst.aa | dd of=/dev/null bs=1M count=20000`.

### 3.6 Overhead

Overhead = time before Clonezilla starts (firmware, DHCP, TFTP, boot of the live system; in `tmpfs` mode
also the download of `filesystem.squashfs`) + reboot after `post.sh`. Time it with a stopwatch on the pilot
(power-on until Clonezilla starts, and `done` until the login prompt). Use 5-10 minutes if you did not
measure it.

---

## 4. Worksheet

| Item | Symbol | Your value | Unit |
| --- | --- | --- | --- |
| Servers deployed at once | `N` | | |
| Image size on the server | `I` | | MB |
| Server network, many targets (3.1) | `Net` | | MB/s |
| Server disk, parallel reads (3.2) | `Disk` | | MB/s |
| Live outgoing rate during pilot (3.3) | `Live` | | MB/s |
| One server alone (3.4) | `T1` | | s |
| Overhead (3.6) | `overhead` | | s |

Calculation:

| Step | Formula | Result |
| --- | --- | --- |
| Server limit | `min(Net, Disk, Live)` | MB/s |
| Server time | `N x I / server_limit` | s |
| Target time | `T1` | s |
| **Deploy time** | `max(server time, target time) + overhead` | s = h |
| Per-server speed | `I / max(server time, target time)` | MB/s |
| Bottleneck | server if server time > target time, else target | |

Quick calculator (edit the first line):

```bash
N=100; I=200000; LIMIT=1100; T1=1620; OVERHEAD=480      # LIMIT = min(Net, Disk, Live)
awk -v N=$N -v I=$I -v L=$LIMIT -v T1=$T1 -v O=$OVERHEAD 'BEGIN{
  s = N*I/L; t = (s > T1 ? s : T1);
  printf "server time %.0f s, target time %.0f s -> bottleneck: %s\n", s, T1, (s > T1 ? "SERVER" : "TARGET");
  printf "deploy time %.0f s = %.2f h (%.0f min)\n", t+O, (t+O)/3600, (t+O)/60;
  printf "speed per server %.1f MB/s of image data\n", I/t }'
```

---

## 5. Worked example

100 servers, image 200 GB on the server (`I` = 200000 MB, about 400 GB of used data at 2:1 compression),
10 GbE server.

| Measurement | Value |
| --- | --- |
| `Net` (15 targets at once, summed) | 1,100 MB/s |
| `Disk` (16 parallel streams) | 3,000 MB/s |
| `Live` (steady during 15-server pilot) | 1,080 MB/s |
| `T1` (one server alone) | 1,620 s (27 min) |
| overhead | 480 s |

```
server_limit = min(1100, 3000, 1080)            = 1,080 MB/s
server_time  = 100 x 200000 / 1080              = 18,519 s   (5.1 h)
target_time  = 1,620 s
deploy_time  = max(18519, 1620) + 480           = 18,999 s   = 5.3 hours
speed        = 200000 / 18519                   = 10.8 MB/s per server  (about 22 MB/s of restored data)
bottleneck   = SERVER (the 10 GbE NIC)
```

The same setup with other server links (`Live` about 94 % of line rate):

| PXE server link | server_limit | server time | deploy time |
| --- | --- | --- | --- |
| 1 GbE | 105 MB/s | 52.9 h | ~53 h |
| 10 GbE | 1,080 MB/s | 5.1 h | ~5.3 h |
| 25 GbE | 2,700 MB/s | 2.1 h | ~2.2 h |
| 100 GbE | 10,800 MB/s | 31 min | ~39 min (now the targets, `T1` 27 min, are close) |

---

## 6. Check the estimate with a pilot

1. Deploy **1** server alone: gives `T1`.
2. Deploy **10-20** servers together while watching `sar -n DEV 5`. Take the steady outgoing MB/s as `Live`.
3. The time of the pilot should match `max(k x I / Live, T1) + overhead`. If the real time is longer, look at
   the causes in 3.3 (nfsd threads, disk `%util`, switch uplink).
4. Compare per-MAC times with `sudo pxe-ctl times`. If the time stays flat as you add servers, the targets
   are the limit. If it grows in proportion to the number of servers, the server network is the limit.

---

## 7. What to change if it is too slow

| Bottleneck | Fix |
| --- | --- |
| Server NIC | faster NIC (25/100 GbE), bonding with a switch that spreads many clients, or several image servers each serving part of the targets |
| Switch uplink / incast | non-oversubscribed uplink to the PXE server, flow control, deploy in waves of about 20 servers |
| Server disk | NVMe or more RAM for the page cache |
| nfsd threads | `threads=64` or more in `/etc/nfs.conf` under `[nfsd]`, then `sudo systemctl restart nfs-server` |
| Image size | clean the source disk before `savedisk`, or image only the OS partition (`saveparts`) |
| Everything (N large) | multicast deployment (Clonezilla udpcast / DRBL): one stream for all targets, so the time is close to `T1`. This tool does not do it today. |
