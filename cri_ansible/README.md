# OS Distribuition
- Ubuntu 26.04

# preinstall

## on the host
```shell
sudo apt update
sudo apt install ansible python3 openssh-client sshpass
ansible --version
```

If Ubuntu cannot locate ansible, enable Universe and retry:

```shell
sudo apt install software-properties-common
sudo add-apt-repository universe
sudo apt update
sudo apt install ansible
```

## on each tester

```shell
sudo apt update
sudo apt install python3 openssh-server bash coreutils
sudo systemctl enable --now ssh
```

# Quick Start

## check and run

```shell
ansible-playbook -i inventory.ini cri_folder_search.yml --syntax-check
ansible servers -i inventory.ini -m ping -k
```

## ready to run

```shell
ansible-playbook -i inventory.ini cri_folder_search.yml -k \
  -e folder_path_A=/data/logs \
  -e fromtime=20261027 \
  -e totime=20261102
```