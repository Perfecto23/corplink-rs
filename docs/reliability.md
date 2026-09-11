# VPN 可靠性与验证合同

VPN 的运行意图、进程存活和隧道可用状态分别记录。日常操作见 [README](../README.md)，术语见 [CONTEXT](../CONTEXT.md)。

## Module 职责与测试 seam

| Module | 调用方 interface | 核心结果 |
| --- | --- | --- |
| 运行监护 | CLI start/status/stop/restart；Rust 运行状态与恢复策略 | 进程身份与代次一致、当前握手才可 ready、取消可收尾、有限恢复、终态通知 |
| VPN 选择与认证会话 | Client 登录/连接；Config 只读加载与会话持久化 | 选中节点与协商目标一致；错误分类；用户配置不自动改写；身份隔离与恢复写入 |
| 网络资源 | NetworkSession 取得/观测/关闭；DNS set/restore | 生产和测试穿过同一生命周期；部分失败回滚；恢复错误可见；原 DNS 可跨异常退出恢复 |
| managed routes | Rust routes/routes-status；解析与已应用记录 | 预检和运行共用规则；无效响应可用匹配缓存；历史应用记录与当前运行分清 |

真实 WireGuard/DNS adapter 和受控操作系统 adapter 使用同一个资源生命周期。测试只替换外部 HTTP、操作系统命令、时间或文件位置，不提供绕过 ready、停止或清理的产品开关。

## 运行结果

- `start` 等待当前子进程的就绪事实；等待超时不伪造后台终态。任何再次启动先核对已有实例，不能覆盖仍存活进程的身份。
- `status` 核对 PID、进程启动标识、运行代次和当前握手年龄；非就绪返回非零。新计算的路由不作为当前已应用证据。
- `stop` 先撤销运行意图，确认进程退出后才完成。失败保留身份和原因；延迟到达的健康更新不能把停止状态改回运行。
- 可恢复传输失败有等待与次数上限。认证失效只执行受控重新登录；需要交互和永久错误保留可解释终态。
- macOS 由 launchd 监护后台 supervisor；终态和崩溃耗尽由 supervisor 通知启动用户，按代次去重。Linux systemd 示例对异常进程终止执行有上限的恢复，不重复拉起已处理的正常终态。

systemd 使用 `on-failure` 并排除应用已处理失败的退出码 `1`，同时保留启动频率限制；这样 Go/Rust panic 的非零退出也会进入崩溃恢复。`on-abnormal` 只覆盖信号、超时等情况，不能替代这个合同。参见 [systemd 的 Restart 合同](https://github.com/systemd/systemd/blob/main/man/systemd.service.xml)。

## 配置与恢复数据

用户配置只读加载；运行中生成的认证数据写入配置旁的私密 sidecar。Cookie 按配置身份隔离，保留原 JSONL 格式。迁移可中断后恢复，损坏内容在恢复写入前保留，原子写失败不破坏旧文件。

macOS DNS 原值在第一次系统修改前以 `0600` 原子快照保存。存在遗留快照时先核对归属并恢复；其他活 owner、未知归属和损坏内容不能被覆盖。恢复部分失败保留快照，成功后才删除。Linux 备份失败禁止覆盖 resolver，恢复失败保留备份。

`last_applied` 绑定配置身份、运行代次、PID 和应用时间，同时保存解析来源、最终 AllowedIPs 与系统路由。Netstack 不安装系统路由。是否仍为当前连接由运行状态另行核对。

managed routes 仅在建立连接时解析；此版本不执行在线路由增删。缓存同时验证来源输入和 TTL，空列表或非法 CIDR 与请求失败经过同一 fallback 判定。

## 构建与验收

Go 桥修复保存在 `libwg/patches/`，在固定 Git source 的临时 archive 副本中应用；不修改 submodule 工作树。补丁或构建失败保留旧库，Rust 在 archive 改变后重新链接。

```bash
libwg/build.sh --test
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
bash -n scripts/corplink-traffic.sh
python3 -m py_compile scripts/*.py
cargo build --release --locked
```

Go C ABI probe 隔离子进程，验证无设备查询、占用端口失败回滚、SOCKS greeting、端口释放和重复启动。它只使用用户态 netstack、回环监听器和临时文件，不配置 peer 或系统 TUN。Rust/CLI 测试使用回环网关和受控 OS adapter。

自动化验证不证明真实 VPN 已安装或可用。真实后台安装、断网/休眠恢复、通知投递以及 Linux/Windows 平台结果需单独记录。服务端自签名证书兼容策略沿用现状；诊断不输出认证秘密。
