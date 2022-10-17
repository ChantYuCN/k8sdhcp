FROM alpine:3.7
USER root

RUN apk --no-cache add dnsmasq

EXPOSE 53/udp

COPY init.sh /usr/local/bin/init.sh
ENTRYPOINT [ "/usr/local/bin/init.sh" ]

