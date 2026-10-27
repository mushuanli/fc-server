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

每台 VM 一个 virtio-fs 导出，可混用多个持久块卷；不支持 DAX、热插拔、快照恢复或 daemon 重连。直通无需文件复制同步，但不提供应用事务、写入冲突合并、文件事件通知保证或凭据隔离。没有进行性能基准、aarch64 guest 验收、实际 LLM 联网调用或现有 nestos-node harness driver 的 launcher 集成。本记录验证的是独立隔离服务及其管理/字节流接口。

## 并发审查修复

本次增加资源预留、同资源串行、慢导入期间查询/停止响应、失败恢复及关闭等待的四项确定性测试。`cargo test --locked` 共 18 项通过，`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 与 release 构建通过。

修复后的 release 已重新通过两套真实 KVM 脚本：块设备场景覆盖读写、只读、持久卷、supervisor/管理进程崩溃回收；virtio-fs 场景覆盖实时读写、源 FD 固定、排除子挂载、宿主只读、启动期限和 backend/supervisor/管理进程崩溃回收。两套日志均确认没有测试密钥泄露，临时 VM/jail/cgroup 已由脚本清理。

当前 server release SHA-256 为 `8a44025a6e1837d22492602c74e03c1540cefe5d6e1a114efecb0dbf5b015c24`；内核、guest 镜像与 virtio-fs VMM/daemon 沿用上表制品。新证据位于 `/var/tmp/firecracker-server-concurrency-block-acceptance.log` 和 `/var/tmp/firecracker-server-concurrency-virtiofs-acceptance.log`，上表旧 server 哈希仅对应最初验收。

## 自动环境构建

`build.sh` 使用相邻 `../firecracker-virtiofs`，完成 musl Firecracker/jailer、Cargo virtiofsd 1.14.0、Linux 6.18.54 和 4096 MiB Debian 13 ext4 构建。内核从清华镜像下载源码并通过固定的 kernel.org SHA-256 校验；guest APT 使用中科大镜像。再次构建复用 ext4，制品摘要写入忽略的 `build/SHA256SUMS`。

- Bash 语法检查、ShellCheck、Python 编译检查通过。
- `python3 scripts/test-build-environment.py`：7 项通过，覆盖重复安装保留配置/密钥、文件权限、失败复制不破坏原制品、符号链接拒绝、既有目录所有权保留及 UID 段冲突。
- 私有 mount namespace 中实际安装、再次安装、`check-environment.py` 全部通过，确认配置和密钥保留；没有在宿主安装或启动长期服务。
- `./build.sh smoke` 使用新内核、根盘及 Cargo daemon 通过完整真实 KVM/virtio-fs 验收：实时读写、源 FD 固定、排除子挂载、宿主只读、启动期限，以及 backend/supervisor/管理进程异常回收；测试 VM、临时目录和 cgroup 已清理，日志未泄露测试密钥。
- guest 内核显式关闭 `CONFIG_FUSE_DAX`。此 fork 没有 DAX shared-memory region，开启时 virtio-fs 驱动探测报 `could not reserve region addr=0x0 len=0x0`，随后挂载报 tag not found。

新构建制品 SHA-256（server、Firecracker/jailer 沿用上述版本）：

| 制品 | SHA-256 |
| --- | --- |
| Linux 6.18.54 vmlinux | `9a2a66b4fb629cf965ef530ab2c85ec5acd18071a28838af3d759363e1f570ff` |
| Debian 13 4096 MiB ext4 | `28d1c8d6222c1c23a533e9e78a38c41dff41144295fc44e0a9ce621ff202c50a` |
| Cargo virtiofsd 1.14.0 | `e42ad646ef58e05f8aecb5b340a590d88964653b35d41cca4da1336f863122c3` |

构建及安装验证日志：`/var/tmp/fc-server-build-environment.log`、`/var/tmp/fc-server-install-environment-test.log`。真实 KVM 日志：`/var/tmp/fc-server-build-environment-smoke.log`。内核配置差异诊断：`/var/tmp/fc-server-build-environment-boot-dax-diagnostic.log`。

## 2 GiB 精简镜像

默认根盘改为 2048 MiB，提供 `base` 与 `harness` 两个构建目标。基础版保留 systemd、udev、Bash、Git、Python、vsock 与网络基础组件；裁剪文档/翻译并保留版权文件，禁用后台维护定时任务、getty 和等待联网，journald 使用最多 16 MiB 的内存日志。运行版另从锁定依赖的构建阶段复制 Node 与 Codex/Claude Code/DSH，最终镜像不含 npm、开发包或构建工具。

`python3 scripts/test-build-environment.py` 共 11 项通过，新增验证 guest 配方、容量和 flavor 改变会使缓存失效、本地 node_modules 不参与配方摘要，以及部署保留稀疏文件和原内容。构建输出原始镜像、输入记录、容量/占用/版本报告、zstd 发布包和 SHA-256。

基础版首轮实测（149 个 Debian 包，不含 Node/三个工具）：文件数据分配约 268.1 MiB，含 ext4 元数据/日志已用 370.2 MiB，宿主稀疏文件占用 271.4 MiB。该版通过完整真实 KVM/virtio-fs 验收，记录创建到 VMM 启动 283 ms，创建到 guest 完成文件读写命令 2849 ms。最终版增加 bubblewrap 后为 150 个 Debian 包，已用 370.4 MiB、宿主占用 271.5 MiB、zstd 包 78.7 MiB。该记录为本机单次样本，包含镜像复制及轮询，不代表性能分位数或真实模型请求耗时。

基础版日志：`/var/tmp/fc-server-lean-base-build.log`、`/var/tmp/fc-server-lean-base-smoke.log`，制品位于忽略的 `build/variants/base/`。

默认运行版使用 Node 24.22.1，预装 Codex 0.164.0、Claude Code 2.1.209、DSH 0.2.0-rc.2。固定依赖通过 BuildKit/npm 镜像源下载并校验 lockfile 的完整性；npm 缓存不进入最终镜像。构建后以 UID 1000、无网络/凭据探测四个版本，确认无 npm、无 Debian 开发包且后台维护定时任务已屏蔽。

| 运行版制品/指标 | 实测 |
| --- | --- |
| 根盘容量 | 2048 MiB |
| ext4 已用（含元数据/日志） | 1668.8 MiB |
| 宿主稀疏文件占用 | 1577.1 MiB |
| zstd 发布包 | 441.3 MiB（462764306 bytes） |
| 创建到 VMM 启动 | 1039 ms |
| 创建到 guest 文件读写命令完成 | 3595 ms |
| guest 内 Node/Codex/Claude/DSH 版本探测 | 30 / 148 / 397 / 95 ms |

运行版通过真实 KVM 块卷和完整 virtio-fs 验收（含持久卷重启后读回、正常关机、只读限制和崩溃回收），四个版本在真实 guest 内均通过。安装与重复安装在私有 mount namespace 再次通过，确认四个绑定目录存在，配置和密钥保留，没有在宿主安装或启动长期服务。再次执行同参数构建，确认命中输入缓存且 ext4 的 inode、mtime 和容量均未改变。Bash、ShellCheck、Python 编译检查通过。没有执行需要模型凭据的真实 LLM 请求。

当前根盘 SHA-256 为 `39f51c3425afdf7c4459e6ce95596636610a203383d839d8266d9a138a9a8455`，其 `.zst` 为 `442e9243b712f9d0f46e9cda6cb0be3a102dffa4e3e146749f80c4fb7d60d98c`。详细输入、容量和版本记录见 `build/images/debian13.ext4.json`，全部制品校验见 `build/SHA256SUMS`。

运行版日志：`/var/tmp/fc-server-lean-harness-build.log`、`/var/tmp/fc-server-lean-harness-smoke.log`、`/var/tmp/fc-server-lean-harness-block-smoke.log`、`/var/tmp/fc-server-lean-install-test.log`、`/var/tmp/fc-server-lean-cache-reuse.log`。

## 已部署网络和 SSH

在用户启动的管理服务上配置 4 个独立网络槽位，guest 使用 `10.203.N.0/30`，veth 使用 `10.204.N.0/30`，通过 `enp1s0` 做 IPv4 NAT。每个 netns 独立运行 systemd 管理的 DHCP/DNS，使用宿主可达的公共 DNS。管理服务、网络与 DNS 单元均已配置开机依赖；未清空或修改 Docker 的现有防火墙表和原有 namespace。

镜像加入 OpenSSH，仅允许 agent 公钥登录，专用私钥位于宿主用户的 `~/.ssh/mindos-firecracker`（0600），公钥由构建脚本注入，输入摘要记录公钥哈希。每个新根盘首次启动独立生成 SSH 主机密钥，客户端使用本次 VM 实例标识区分复用的 IP。根盘仍为 2147483648 bytes，新增 SSH 后 zstd 包为 466018848 bytes，记录见当前 `build/SHA256SUMS` 和 `debian13.ext4.json`。

- `python3 scripts/test-build-environment.py`：15 项通过；新增私钥不可进入镜像、64 个槽位的地址/MAC 独立、loopback DNS 拒绝、宿主路由冲突拒绝。
- nftables 两层规则经过 `nft --check` 校验并实际应用。SSH/PTY、DNS、公网 HTTPS 在两个临时 VM 上通过；两台 VM 的主机密钥不同。
- guest 到宿主 `192.168.31.10`、路由器 `192.168.31.1`、metadata `169.254.169.254` 和另一台 VM 的连接均被阻断。
- 两台测试 VM 在 POST 返回后约 2381 / 2378 ms 可通过 SSH 执行命令，测试后均正常关机回收。测试记录不包含真实模型认证请求。
- `./build.sh start/ssh/stop` 已由普通宿主用户实测，guest 为 UID 1000，CLI 测试 VM 正常关机。为用户保留的 `demo` 挂载 project，地址 `10.203.0.2`，三个运行时版本验证通过。
- 管理服务最初启动后的双 VM 根盘测试也通过：A 写入文件后 B 不存在该文件，B 写同名文件不改变 A；两台均正常关闭。

日志：`/var/tmp/fc-server-ssh-image-build.log`、`/var/tmp/fc-server-network-deploy.log`、`/var/tmp/fc-server-network-check.log`、`/var/tmp/fc-server-network-smoke.log`。当前实际部署已包含网络/SSH，与前文仅在 mount namespace 内测试安装的阶段不同。

## 2026-10-25：共享根盘物理内存

已实施并部署 `virtio-pmem + ext4 DAX + OverlayFS`。复用 fork 已有 PMEM 设备，不修改其 virtio-fs 协议；基础镜像为 root:root / 0444，同一 inode 硬链接进各个 jail，并使用只读 KVM 内存槽。每 VM 的私有 ext4 upper 按需占用磁盘，默认容量 2 GiB，随 VM 清理。私有副本模式继续支持从只读基础镜像创建可写根盘。

- `FC_SHARED_TEST_VMS=100` 真实 KVM 验收通过：100 个同时运行的 VM，每个 1 vCPU / 256 MiB，无网；第一个 VM 同时使用 live virtio-fs 项目目录。全部 VM 完成 Node 文件 DAX mmap、读取所有文件页，以及一个私有 mmap 页的修改。
- Node 文件大小 126879696 bytes；每个 guest pagemap 检查到 30569 个 PMEM 页、408 个稀疏文件零页和 1 个私有 COW 页。`statx` 确认基础文件启用 DAX，零页通过 guest `kpageflags` 的 `KPF_ZERO_PAGE` 识别，避免把零页误算为未共享的文件页。
- 宿主 pagemap 对比全部 100 个 VMM：采样时共同驻留的基础镜像页共 **87027712 bytes（82.996 MiB）**，对应偏移处 PFN 差异为 0；各 VM 的基础映射驻留量为 83.023..160.719 MiB。共同驻留量随回收变化，这不是整张 2 GiB 镜像的内存占用，也不是最大共享量。
- 两 VM 的最终回归采样共同驻留 **180617216 bytes（172.25 MiB）**，对应 PFN 差异为 0。修改同一 `/home/agent/.bashrc` 后，各自读取到自己的内容；guest root 直接尝试写 `/dev/pmem0` 后，宿主基础镜像 SHA-256 保持不变。停止并重建 VM 后，私有修改消失。
- 100 个 VM 全部正常关机回收。该测试验证基础系统、共享 mmap 和隔离；未运行 100 个并发 Codex/Claude/DSH 模型会话。guest 内核、堆、栈、普通 read 缓冲和 OverlayFS copy-up 仍独立占用资源；文件 copy-up 是整文件复制。
- `cargo test --locked`：4 个库测试及 15 个边界测试通过。构建/部署 Python 回归 17 项通过，包含迁移幂等、保留网络配置、自定义镜像拒绝、100 槽位独立性。ShellCheck、Bash/Python 语法及真实 KVM 块卷、virtio-fs 完整回归通过。
- 本机已通过 `./build.sh shared-root` 切换内置 profile，原配置备份到 `/etc/mindos-firecracker/config.before-shared-root.toml`；API key、目录绑定和 4 个网络槽位核对一致。服务保持启动并开机启用；日常并发上限仍为 4，程序支持配置到 128。
- 部署后的共享模式通过 SSH/PTY、不同主机密钥、DNS/HTTPS、宿主/私网/metadata/跨 VM 阻断验收；两个 VM 创建后约 2402 / 2405 ms 可执行 SSH 命令，随后正常回收。
- 最后从已部署共享根盘经 SSH 执行镜像检查：Node v24.22.1、Codex 0.164.0、Claude Code 2.1.209、DSH 0.2.0-rc.2 均通过，版本探测约 7 / 65 / 186 / 80 ms。测试 VM 正常关闭，没有留下活动 VM。
- 两 VM 组合验收同时挂载 PMEM 共享根盘、live virtio-fs 和独立 virtio-blk 持久卷；停止并重建后，根盘私有修改消失，持久卷中的 `durable` 标记保留。此项验证使用独立测试目录，没有读写生产绑定数据。

新 initramfs 为 1063646 bytes，SHA-256 `9e931aa967ec13b6a4543c2c019147149055546e0d0d14b4a140f6332acd2cd5`；内核 SHA-256 仍为 `9a2a66b4fb629cf965ef530ab2c85ec5acd18071a28838af3d759363e1f570ff`。基础镜像容量 2147483648 bytes，文件系统已用 1763086336 bytes，zstd 包 467108564 bytes；完整版本、输入和哈希见 `build/images/debian13.ext4.json`、`build/SHA256SUMS`。

日志：`/var/tmp/fc-shared-100-smoke.log`、`/var/tmp/fc-shared-regression-smoke.log`、`/var/tmp/fc-shared-persistent-smoke.log`、`/var/tmp/fc-shared-final-build.log`、`/var/tmp/fc-shared-deploy.log`、`/var/tmp/fc-shared-network-smoke.log`、`/var/tmp/fc-shared-environment-check.log`、`/var/tmp/fc-shared-runtime-check.log`。

## 2026-10-25：nftables TCP/UDP 端口发布

已实现并部署 `./build.sh publish VM HOST:GUEST[/tcp|udp] [--listen IPv4]`、`unpublish VM HOST[/tcp|udp]` 和 `ports VM`。默认只绑定 127.0.0.1，显式指定 0.0.0.0 时允许宿主所有 IPv4 接口入站；两端端口为 1..65535，每 VM 最多 16 条。发布使用独立管理 API，profile 的 `ports` 继续表示 vsock 授权。

- 真实 VM 与原生 CLI 验收通过：宿主 loopback 的 TCP/UDP，以及隔离外部客户端 netns 经宿主 `198.18.255.1` 的 TCP/UDP，均到达正确 guest 的 8080 服务；绑定 loopback 的映射不能从这个外部网段访问。
- 本机应用已占用端口、另一个 VM 的 wildcard 端口均被拒绝为 `HOST_PORT_IN_USE`；相同映射重复发布不增加记录。管理服务保留绑定 socket，但转发数据直接走内核 DNAT/SNAT，不经过代理进程。
- 未发布的 guest 8081 服务仍被阻断；guest 访问宿主发布端口仍被私网隔离阻断。撤销 TCP/UDP 映射后不可访问，API/CLI 查询为空。
- 同 VM ID、同 IP 槽位重建验证通过：旧规则没有恢复；复用同一个 UDP 客户端 socket，在显式重新发布前无法访问新 guest，重新发布后才得到新实例的标记。新增映射会定向清理访问该监听端口产生的无 NAT 连接，修复旧 UDP conntrack 导致的新规则不生效问题。
- 映射删除先撤销 active conntrack mark，再清理对应 root NAT flow；VM 停止还清空所属 netns 的连接跟踪，然后才释放 UID/IP 槽位。mark 前缀为 `0xfc000000/0xff000000`，只在本服务的受管接口处理。更新仅重建有变化的 netns 规则，服务内串行执行 nft 操作，不持有全局资源目录锁等待子进程。
- 管理服务 SIGKILL 故障注入通过：supervisor 回收 VMM，systemd ExecStopPost 清除发布记录和规则；测试只清理自身已退出 VM 的遗留 jail/cgroup 后重启服务。新建同 ID/IP 的 VM 没有旧映射，明确重新发布后 TCP/UDP 正常。故障注入需使用脚本的 `--crash`，并要求管理服务没有业务 VM。
- Python 10 项发布回归通过，包含规则作用域、wildcard 冲突、实例隔离、无 NAT UDP 清理、变更 netns 选择、发布重试幂等、应用失败回滚和回滚失败时撤销 active mark/保留恢复日志。已有构建部署回归 17 项通过。Rust 7 项库测试及 15 项边界测试通过，包含绑定保留/释放、TCP/UDP 独立冲突与请求字段约束。
- 原有 SSH/PTY、独立主机密钥、公网 DNS/HTTPS 和宿主/私网/metadata/跨 VM 阻断回归通过。对比变更前后的 nftables JSON，除本服务拥有的两个表及实时计数外，宿主/Docker 规则保持一致。ShellCheck、Bash/Python 语法、Rust fmt 和 diff 空白检查通过。

当前映射配置只对运行中的 VM 实例生效；服务/VM 重建后不自动恢复。IPv6 和端口范围未实现。为适配 netns 的输入白名单，guest 看到 SNAT 后的槽位宿主地址 `10.204.N.1`，不保留客户端原始 IP。第三方纯 DNAT 规则没有监听 socket 时，需要管理员协调端口分配。

服务保持 active/enabled，测试 VM、模拟外部客户端 namespace 和端口映射已清理。日志：`/var/tmp/fc-publish-final-build.log`、`/var/tmp/fc-publish-final-install.log`、`/var/tmp/fc-publish-rust.log`、`/var/tmp/fc-publish-python.log`、`/var/tmp/fc-publish-final-smoke.log`、`/var/tmp/fc-publish-network-regression.log`、`/var/tmp/fc-publish-check.log`。
