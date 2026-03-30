# Server ISO file over http with Apache

```console
sudo apt install apache2
ISO_DIR="/var/www/html/iso"
sudo mkdir -p ${ISO_DIR}
sudo chown -R www-data:www-data ${ISO_DIR}
sudo chmod -R 755 ${ISO_DIR}
```

```console
VHOST_CONF="/etc/apache2/sites-available/iso.conf"
cat > ${VHOST_CONF} << EOL
<VirtualHost *:8001>
    ServerAdmin webmaster@localhost
    DocumentRoot /var/www/html/iso

    ErrorLog \${APACHE_LOG_DIR}/iso-error.log
    CustomLog \${APACHE_LOG_DIR}/iso-access.log combined

    ProxyPreserveHost On
    ProxyPass / http://192.168.6.1:8001/
    ProxyPassReverse / http://192.168.6.1:8001/
</VirtualHost>
EOL
```

```console
sudo systemctl restart apache2
```
