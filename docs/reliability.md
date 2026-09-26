# VPN 可靠性与验证合同

VPN 的运行意图、进程存活和隧道可用状态分别记录。日常操作见 [README](../README.md)，术语见 [CONTEXT](../CONTEXT.md)。

## Module 职责与测试 seam

| Module | 调用方 interface | 核心结果 |
| --- | --- | --- |
| 运行监护 | CLI start/foreground/status/stop/restart；Rust 运行状态与恢复策略 | 进程身份与代次一致、当前握手才可 ready、取消可收尾、有限恢复、终态通知 |
| VPN 选择与认证会话 | Client 登录/连接；Config 只读加载与会话持久化 | 选中节点与协商目标一致；错误分类；用户配置不自动改写；身份隔离与恢复写入 |
| 网络资源 | NetworkSession 取得/观测/关闭；DNS set/restore | 生产和测试穿过同一生命周期；部分失败回滚；恢复错误可见；原 DNS 可跨异常退出恢复 |
| managed routes | Rust routes/routes-status；解析与已应用记录 | 预检和运行共用规则；无效响应可用匹配缓存；历史应用记录与当前运行分清 |

真实 WireGuard/DNS adapter 和受控操作系统 adapter 使用同一个资源生命周期。测试只替换外部 HTTP、操作系统命令、时间或文件位置，不提供绕过 ready、停止或清理的产品开关。

实现入口：[main.rs](../src/main.rs) 的 `supervise_session` 统一处理已建立资源的会话；[runtime.rs](../src/runtime.rs) 维护运行观测与恢复策略；[network_session.rs](../src/network_session.rs) 负责资源取得和清理；[操作脚本](../scripts/corplink-traffic.sh) 负责进程监护与用户命令。认证与路由分别由 [client.rs](../src/client.rs)、[managed_routes.rs](../src/managed_routes.rs) 提供。

## 运行结果

- `start` 等待当前子进程的就绪事实；等待超时不伪造后台终态。任何再次启动先核对已有实例，不能覆盖仍存活进程的身份。
- `status` 核对 PID、进程启动标识、运行代次和当前握手年龄；非就绪返回非零。新计算的路由不作为当前已应用证据。
- `stop` 先撤销运行意图，确认进程退出后才完成。失败保留身份和原因；延迟到达的健康更新不能把停止状态改回运行。
- 前后台共享运行身份和单实例检查；操作互斥使用进程退出时释放的内核锁。锁文件存在不表示正在执行操作；旧目录锁只在核实 owner 已退出后回收。
- Netstack 使用现有公司登录域名探测隧道内 DNS（server 为 IP 时不探测）；每次最多 5 秒，并给剩余 DNS 服务器预留查询时间，避免首选服务器耗尽备用服务器的预算；不修改系统 DNS。首次 DNS 失败不能发布 ready；运行中首次失败发布 degraded，连续 3 次失败重建连接，成功探测清零连续计数。SOCKS 域名解析也受 5 秒超时约束；`socks5_dns_tcp=true` 时两者均使用隧道内 TCP DNS。
- 首次握手失败和后续健康失败共用收尾路径。本地资源清理失败进入终态，不继续重连，也不报告停止成功。
- 可恢复传输失败有等待与次数上限。认证失效只执行受控重新登录；需要交互和永久错误保留可解释终态。
- macOS 由 launchd 监护后台 supervisor；终态和崩溃耗尽由 supervisor 通知启动用户，按代次去重。Linux systemd 示例对异常进程终止执行有上限的恢复，不重复拉起已处理的正常终态。

CLI 的 `ready` 是当前子进程的握手与身份观测，`start` 还对 TUN 探测目标核对路由；它们不代表所有目标服务都可访问。`status` 使用已发布观测，不重新解析源地址；实时路由、TCP 和 Git 请求分别通过 `test-host`、带 `TEST_PORT` 的 `test-host` 和 `test` 验证。

前台会话保留交互输入，也向同一运行目录发布身份。输入通过标准输入穿过 sudo，提权后的 supervisor 再为异步子进程建立输入描述符。后台 supervisor 不继承操作锁；前台登记完成后释放操作锁，使另一终端能够查询或停止。

systemd 使用 `on-failure` 并排除应用已处理失败的退出码 `1`，配合 `RestartSec=5s`、300 秒内最多 3 次启动限制。CLI 的 macOS 后台模式由 launchd 管理 supervisor，已处理终态由 supervisor 正常退出以结束外层恢复。Shell 的 Linux `process` 模式不提供 supervisor 自身的 OS 重启保障；持续运行需使用 systemd。参见 [systemd 的 Restart 合同](https://github.com/systemd/systemd/blob/main/man/systemd.service.xml)。

## 上游稳定性修复

本 fork 移植了 [上游 #97 / 90370cf](https://github.com/PinkD/corplink-rs/commit/90370cf07b414184e2c0ae285f2b75865a861165) 的节点发现、并发探测、Cookie 隔离与 IPv6 处理。默认策略仍按服务端列表优先级选节点；探测响应不会修改共享认证状态，只有最终选中节点的 Cookie 会用于协商和持久化。所有候选失败时保留错误分类，由既有恢复策略处理传输失败、服务端错误和认证失效；探测错误不输出响应体。

服务端未分配 IPv6 隧道地址时忽略其 IPv6 路由；VPN 列表请求携带与 User-Agent 一致的 app version。`managed_routes`、`extra_allowed_ips`、认证 sidecar、有限重连和本地 Go 传输补丁继续使用本 fork 的实现。本次未引入上游的额外路由配置字段，也未同步其依赖锁文件或发行版本号。

## 配置与恢复数据

用户配置只读加载；运行中生成的认证数据写入配置旁的私密 sidecar。Cookie 按配置身份隔离，保留原 JSONL 格式。迁移可中断后恢复，损坏内容在恢复写入前保留，原子写失败不破坏旧文件。

| 数据 | 默认位置 | 权威与保留方式 |
| --- | --- | --- |
| 用户配置 | Shell 入口默认 `config.local.json` | 用户维护；预检和运行不自动回写 |
| 认证会话 | 配置旁的 `<interface>_session.json` | 设备信息、key、登录状态；Unix 写入使用 `0600` |
| Cookie | 配置旁含身份标识的 `*_cookies.jsonl` | 按配置身份隔离；Unix 写入与恢复备份使用 `0600` |
| 当前运行观测 | `.run/corplink-runtime.json` | CLI/supervisor 发布运行身份，Rust 发布握手及终态；原子写入固定 `0644`，不含认证秘密 |
| 进程辅助与操作锁 | `.run/` 中的 PID、stop marker、`corplink-traffic.lockfile` | 不独立证明隧道可用；内核锁的持有状态不由文件是否存在判断 |
| 诊断日志 | `.run/corplink-traffic.log`、`.run/corplink-runtime-events.log` | 重启追加，记录故障与状态变化；前台连接输出同时以终端为主要查看入口 |
| managed routes cache | 配置目录下 `.run/managed-routes-cache.json` | 按来源输入与 TTL 决定是否可用 |
| 上次应用记录 | cache 旁的 `managed-routes-cache.status.json` | `last_applied` 历史证据，不能单独说明连接仍在运行 |

`CORPLINK_RUN_DIR` 改变监护文件位置；cache 路径由 `managed_routes.cache_file` 单独配置。认证身份包含规范化配置路径、公司、账号、平台和声明的 server；相对路径与绝对路径指向同一配置时使用同一身份。更换身份不复用旧 cookie，保留旧文件不等于继续使用它。

状态 JSON 损坏时返回明确错误并保留原文件，不在持锁期间重复解析。认证文件的 `0600` 与诊断状态的 `0644` 是不同合同；严格 `umask` 或 root/普通用户交替操作不能改变诊断状态的可读性。

macOS DNS 原值在第一次系统修改前以 `0600` 原子快照保存。存在遗留快照时先核对归属并恢复；其他活 owner、未知归属和损坏内容不能被覆盖。恢复部分失败保留快照，成功后才删除。Linux 备份失败禁止覆盖 resolver，恢复失败保留备份。

`last_applied` 绑定配置身份、运行代次、PID 和应用时间，同时保存解析来源、最终 AllowedIPs 与系统路由。Netstack 不安装系统路由。是否仍为当前连接由运行状态另行核对。

managed routes 仅在建立连接时解析；此版本不执行在线路由增删。缓存同时验证来源输入和 TTL，空列表或非法 CIDR 与请求失败经过同一 fallback 判定。

## 构建与验收

Go 桥修复保存在 `libwg/patches/`，在固定 Git source 的临时 archive 副本中应用；不修改 submodule 工作树。Bash 与 PowerShell 构建入口均在 patched source 目录执行 Go 测试和构建。补丁或构建失败保留旧库，Rust 在 archive 改变后重新链接。

```bash
libwg/build.sh --test
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
bash -n scripts/corplink-traffic.sh
python3 -m py_compile scripts/*.py tests/*.py
cargo build --release --locked
```

Go C ABI probe 隔离子进程，验证无设备查询、占用端口失败回滚、SOCKS greeting、端口释放、重复启动，以及没有可用 DNS 路径时的真实 C ABI 探测超时。它只使用用户态 netstack、回环监听器和临时文件，不配置 peer 或系统 TUN。Rust/CLI 测试使用回环网关和受控 OS adapter。

TCP 传输的连续包测试位于补丁中的 `conn/bind_tcp_burst_test.go`，通过真实回环 TCP 验证包内容不被后续读取覆盖。接收缓冲区在消费方复制完成后归还池；发送端串行写入完整帧，避免握手与数据并发发送时帧头、包体交错。`conn/bind_tcp_concurrent_test.go` 使用真实回环 TCP 验证并发帧完整性；`--test` 同时运行 `conn` 和 `tun/netstack` 包；DNS 回归包含首选黑洞、备用成功和全部黑洞按期失败，连续包回归自身也有超时收尾。API 响应体中断归类为可恢复传输错误；VPN 协商请求使用 30 秒上限，普通 API 保持原有 10 秒上限。

关键回归入口：

- [main.rs](../src/main.rs)：通过生产 `supervise_session` 验证首次握手和后续健康失败、清理失败阻止重连、清理成功允许重试、停止结果。
- [test_runtime_cli.py](../tests/test_runtime_cli.py)：无效状态、锁争用与旧锁恢复、前后台单实例、stdin、Ctrl-C、异步退出、sudo 关闭额外描述符和严格 `umask`。
- [test_packaged_cli.py](../tests/test_packaged_cli.py)：Release 布局中的 Shell/Python 入口使用包内 binary，预检不生成运行状态。
- [libwg_ffi_probe.py](../tests/libwg_ffi_probe.py)：真实 C ABI 的 netstack 取得、失败和释放。

自动化验证不证明真实 VPN 已安装或可用。真实后台安装、断网/休眠恢复、通知投递以及 Linux/Windows 平台结果需单独记录。服务端自签名证书兼容策略沿用现状；诊断不输出认证秘密。
