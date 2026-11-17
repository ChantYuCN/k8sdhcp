. Measure the real speeds
Don't trust the nominal link speeds. Measure instead.

What	How
Network between two machines	On the server run iperf3 -s. On a target run iperf3 -c SERVER_IP. Run 10 targets at once and add up the results.
Server disk read	dd if=/home/partimag/cri-golden/<big file> of=/dev/null bs=1M
Target disk write	dd if=/dev/zero of=/dev/nvme0n1 bs=1M count=20000 oflag=direct (this erases data; only on a disk you can lose)
Live NFS load during a deploy	On the server: sar -n DEV 5 or nload. Look for the NIC pinned near its limit.
Restore speed on one target	The Clonezilla progress line shows MB/min. Divide by 60 for MB/s.
