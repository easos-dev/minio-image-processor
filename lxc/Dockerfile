# LXC 容器模板：在已构建的网关镜像之上加 systemd 与网络工具，导出根文件系统就是 Proxmox / LXC 的容器模板。
# 只在打模板时用（scripts/build-lxc-template.sh），不是运行镜像；BASE 指向 Dockerfile 构建出的网关镜像。
ARG BASE=easit2/minio-image-processor:latest
FROM ${BASE}
USER root
ENV DEBIAN_FRONTEND=noninteractive
# systemd-sysv 提供 /sbin/init（LXC 的 PID 1）；ifupdown 与 DHCP 客户端让 Proxmox 写进 /etc/network/interfaces 的网络配置生效。
RUN apt-get update \
 && apt-get install -y --no-install-recommends systemd systemd-sysv ifupdown isc-dhcp-client iproute2 \
 && rm -rf /var/lib/apt/lists/* \
 # 模板里不能带固定的 machine-id：留空文件，每个容器首次启动时自己生成。
 && truncate -s 0 /etc/machine-id \
 && rm -f /var/lib/dbus/machine-id
COPY lxc/image-processor.service /etc/systemd/system/image-processor.service
# 配置与 config.example.toml 相同，部署后改 upstream 再重启服务。
COPY config.example.toml /etc/image-processor/config.toml
RUN chmod 644 /etc/image-processor/config.toml /etc/systemd/system/image-processor.service \
 && systemctl enable image-processor.service
# 模板的 PID 1 是 systemd；运行镜像的 ENTRYPOINT 在这里不再有意义。
ENTRYPOINT ["/sbin/init"]
