FROM debian:13
ARG DEBIAN_MIRROR=http://deb.debian.org/debian
ARG DEBIAN_SECURITY_MIRROR=http://deb.debian.org/debian-security
RUN sed -i "s|http://deb.debian.org/debian-security|${DEBIAN_SECURITY_MIRROR}|g; s|http://deb.debian.org/debian|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    systemd systemd-sysv systemd-resolved dbus socat ca-certificates bash coreutils util-linux \
    git python3 nodejs npm iproute2 && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 --shell /bin/bash agent \
    && passwd -l root && passwd -l agent \
    && mkdir -p /workspace /etc/systemd/network /usr/local/libexec \
    && systemctl mask console-getty.service serial-getty@ttyS0.service getty@tty1.service \
    && systemctl enable systemd-networkd.service systemd-resolved.service \
    && ln -sf /lib/systemd/system/reboot.target /etc/systemd/system/ctrl-alt-del.target
COPY mount-bindings /usr/local/libexec/mount-bindings
COPY harness-7000 /usr/local/libexec/harness-7000
COPY mindos-bindings.service /etc/systemd/system/mindos-bindings.service
COPY harness-vsock@.service /etc/systemd/system/harness-vsock@.service
COPY 10-eth.network /etc/systemd/network/10-eth.network
RUN chmod 0755 /usr/local/libexec/mount-bindings /usr/local/libexec/harness-7000 \
    && systemctl enable mindos-bindings.service harness-vsock@7000.service \
    && truncate -s 0 /etc/machine-id
CMD ["/sbin/init"]
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends udev \
    && rm -rf /var/lib/apt/lists/*
