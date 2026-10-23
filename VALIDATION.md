# Firecracker server 验收记录

日期：2026-10-24。环境：Linux x86_64、真实 `/dev/kvm`、cgroup v2、Debian 13 guest、Linux 6.18.54 guest 内核、Rust virtiofsd 1.14.0。测试仅使用临时授权目录和根盘副本；没有连接真实 Codex 凭据或 session。

## 构建与契约

- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过。
- `cargo test --locked`：14 项通过，覆盖认证、Origin、参数/路径注入、目录授权与互斥、只读块设备、vsock 握手与 stdin EOF、真实 ext4 导入和目录替换后的 FD 固定。
- `cargo build --release --locked` 通过。
- Shell/Python 脚本语法检查通过。
- `prepare-firecracker-virtiofs.sh` 从上游拉取固定基线并应用补丁，20 个修改文件与已构建的 fork 逐字节一致。

## 真实 KVM

以下两组脚本使用表内 server release 二进制均通过，最终测试进程、临时 jail 和 VM/backend cgroup 已回收。

`scripts/smoke.py` 覆盖：Debian 13/agent UID 1000、可写卷、只读卷、正常关机后重启读取未手工 sync 的文件、ext4 流式导出、supervisor 被 SIGKILL 后回收 VMM、管理进程被 SIGKILL 后回收 VMM。正常关闭必须返回 `mode=guest-shutdown`，强停不会被视为落盘成功。

`scripts/smoke-virtiofs.py` 使用 fork 的 musl release 和默认 seccomp，覆盖：

- 宿主目录直接读写、rename、session 文件落宿主，宿主修改能被 guest 读取。
- 服务固定源 FD 后将原目录改名、替换原路径为符号链接，仍只导出原目录。
- 非递归 clone 排除宿主子挂载；指向导出范围外的符号链接不能读取宿主文件。
- 并发导出同目录或直接 bind 子目录形成的别名返回 409。
- 在一次性测试镜像里由 guest root 把只读 virtio-fs remount 为 rw，create/unlink/rename 仍被宿主只读授权拒绝。
- vhost-user backend 接受连接但不回复时，创建在共享启动期限内失败，VMM/daemon 全部回收。
- backend 崩溃、分别 SIGKILL VMM supervisor 和 backend supervisor、SIGKILL 管理服务后，检查进程组/cgroup 为空。
- 管理日志不包含测试 API key。

原始设备层测试另在 fork 中通过：启动后配置拒绝、创建快照前拒绝且不留下快照文件、daemon 断开令 VMM 非零退出；管理服务不提供快照或热插拔接口。

## 本机证据与制品

服务验收摘要保存于 `/var/tmp/firecracker-server-block-acceptance.log`、`/var/tmp/firecracker-server-virtiofs-acceptance.log`。设备层证据在 `/home/li/share/prj/virtiofs-validation/final-keyboard/`。系统日志和测试源文件不随仓库提交。

| 制品 | SHA-256 |
| --- | --- |
| server release | `4d52590e2e99dd1ffde356f8c24f47d1ea44d1dab33b4b02ec52f187d14741b9` |
| Firecracker fork musl release | `2e67c23d06a8320af90aa610a06b30fc57a2fb4191258c00494b9db94db68e27` |
| 对应 jailer | `058be3eae177c0aa11b2c038b31f774ad0e5a19a4272adfdf6a8231890560b96` |
| Linux 6.18.54 vmlinux | `dbbdab9311284c3107c173abe685f576106e844d85d437879727dd8af45e7f34` |
| Debian 13 完整服务镜像 | `85e0b237ba7f1ceb77af5f716721b442dfb3dcc0f47bba032f18133573eab872` |
| Rust virtiofsd | `2272d8bc4502b2abe2d6509b2bc0db6af8cd1cf02de9628fc883ef4ca6fcfd60` |
| 仓库 VMM 补丁 | `4112acb7ae58bfd5283ec9006c235e274cdaa34e8b1393080719e199ed71664b` |

Debian 制品位于 `/var/tmp/mindos-firecracker-debian13-clean.ext4`。基础镜像中的 Docker 标记必须在 export 后删除；否则 systemd 误判容器而跳过完整的 VM 卸载流程。`acpi=off` 下使用正常 reboot/i8042 reset 退出 VMM，不能依赖 poweroff/halt。

## 当前范围

每台 VM 一个 virtio-fs 导出，可混用多个持久块卷；不支持 DAX、热插拔、快照恢复或 daemon 重连。直通无需文件复制同步，但不提供应用事务、写入冲突合并、文件事件通知保证或凭据隔离。没有进行性能基准、aarch64 guest 验收、实际 LLM 联网调用或现有 mindos-node harness driver 的 launcher 集成。本记录验证的是独立隔离服务及其管理/字节流接口。

## 并发审查修复

本次增加资源预留、同资源串行、慢导入期间查询/停止响应、失败恢复及关闭等待的四项确定性测试。`cargo test --locked` 共 18 项通过，`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 与 release 构建通过。

修复后的 release 已重新通过两套真实 KVM 脚本：块设备场景覆盖读写、只读、持久卷、supervisor/管理进程崩溃回收；virtio-fs 场景覆盖实时读写、源 FD 固定、排除子挂载、宿主只读、启动期限和 backend/supervisor/管理进程崩溃回收。两套日志均确认没有测试密钥泄露，临时 VM/jail/cgroup 已由脚本清理。

当前 server release SHA-256 为 `8a44025a6e1837d22492602c74e03c1540cefe5d6e1a114efecb0dbf5b015c24`；内核、guest 镜像与 virtio-fs VMM/daemon 沿用上表制品。新证据位于 `/var/tmp/firecracker-server-concurrency-block-acceptance.log` 和 `/var/tmp/firecracker-server-concurrency-virtiofs-acceptance.log`，上表旧 server 哈希仅对应最初验收。
