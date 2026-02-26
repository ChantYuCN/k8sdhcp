# Purpose 
Use Kubernetes pod to run dhcp, discover and assign ip to Node

========          ======== 
| k8s  |          |      |
| pod  |---dhcp---| node |
| node |          |      |
========          ========

- make image
```console 
$ docker build . -t localdhcp:v1 --build-arg HTTP_PROXY=$http_proxy --build-arg HTTPS_PROXY=$https_proxy --rm --file Dockerfile 
$ helm install dhcp ./dnsmasq -n dhcp --create-namespace
```


```console
# A simple script to install deb package on ubuntu to provide dhpc service
bash linuxshell.sh 
```
