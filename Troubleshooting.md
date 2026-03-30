# Lesson 1

Failed to set DNS configuration: Unit dbus-org.freedesktop.resolve1.service not found.  

# Solution 1

```console
sudo systemctl start systemd-resolved.service
sudo systemctl enable systemd-resolved.service  
```

# Lesson 2 

 Failed to set DNS configuration: Link lo is loopback device.  

# Solution 2

Modify /etc/default/dnsmasq 

```console
IGNORE_RESOLVCONF=yes
DNSMASQ_EXCEPT="lo"
```

