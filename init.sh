#!/bin/sh

while (! ls /etc/dnsmasq.conf > /dev/null 2>&1 ); do
  echo \"Waiting for dnsmasq.conf file to be created\";
  sleep 5;
done

exec dnsmasq --conf-file=/etc/dnsmasq.conf -d --log-dhcp --log-queries=extra
~

