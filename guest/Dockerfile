ARG NODE_IMAGE=node:24.22.1-trixie-slim
FROM ${NODE_IMAGE} AS runtime-builder
ARG NPM_REGISTRY=https://registry.npmjs.org
WORKDIR /opt/harness
COPY runtimes/package.json runtimes/package-lock.json ./
RUN --mount=type=cache,id=fc-server-npm,target=/root/.npm,sharing=locked \
    npm ci --omit=dev --no-audit --no-fund --registry="${NPM_REGISTRY}"

FROM debian:13-slim AS base
ARG DEBIAN_MIRROR=http://deb.debian.org/debian
ARG DEBIAN_SECURITY_MIRROR=http://deb.debian.org/debian-security
COPY dpkg-excludes /etc/dpkg/dpkg.cfg.d/01-mindos-small
RUN sed -i "s|http://deb.debian.org/debian-security|${DEBIAN_SECURITY_MIRROR}|g; s|http://deb.debian.org/debian|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    systemd systemd-sysv systemd-resolved dbus udev socat ca-certificates bash coreutils util-linux \
    git python3 bubblewrap openssh-server iproute2 libstdc++6 libatomic1 busybox-static \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/* \
    && useradd --create-home --uid 1000 --shell /bin/bash agent \
    && passwd -l root && passwd -l agent \
    && mkdir -p /workspace /etc/systemd/network /usr/local/libexec /usr/share/mindos-image
COPY ssh-authorized-keys /home/agent/.ssh/authorized_keys
COPY mount-bindings harness-7000 configure-system image-check /usr/local/libexec/
COPY mindos-bindings.service harness-vsock@.service mindos-ssh-hostkey.service /etc/systemd/system/
COPY ssh.conf /etc/ssh/sshd_config.d/mindos.conf
COPY ssh-unit.conf /etc/systemd/system/ssh.service.d/mindos.conf
COPY 10-eth.network /etc/systemd/network/10-eth.network
COPY journald.conf /etc/systemd/journald.conf.d/mindos.conf
RUN chmod 0755 /usr/local/libexec/mount-bindings /usr/local/libexec/harness-7000 \
    /usr/local/libexec/configure-system /usr/local/libexec/image-check \
    && /usr/local/libexec/configure-system \
    && printf 'base\n' > /usr/share/mindos-image/flavor
ENV LANG=C.UTF-8 LC_ALL=C.UTF-8
CMD ["/sbin/init"]

FROM base AS harness
ENV DISABLE_AUTOUPDATER=1
COPY --from=runtime-builder /usr/local/bin/node /usr/local/bin/node
COPY --from=runtime-builder /opt/harness /opt/harness
RUN ln -s /opt/harness/node_modules/.bin/codex /usr/local/bin/codex \
    && ln -s /opt/harness/node_modules/.bin/claude /usr/local/bin/claude \
    && ln -s /opt/harness/node_modules/.bin/dsh /usr/local/bin/dsh \
    && cp /opt/harness/package-lock.json /usr/share/mindos-image/runtime-lock.json \
    && printf 'harness\n' > /usr/share/mindos-image/flavor
