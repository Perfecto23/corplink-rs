# corplink-rs 使用说明

`corplink-rs` 是 Rust VPN 客户端，可把 GitHub、Redshift 等有 IP 白名单限制的目标通过公司 VPN 出口访问。本文以 macOS 日常使用为主，Linux 和 Windows 的入口见平台范围一节。

日常只需要维护本机的 `config.local.json`，然后运行：

```bash
scripts/corplink-traffic.sh start
```

仓库中的维护文档：[项目约定](https://github.com/Perfecto23/corplink-rs/blob/master/AGENTS.md)、[领域术语](https://github.com/Perfecto23/corplink-rs/blob/master/CONTEXT.md)、[可靠性合同](https://github.com/Perfecto23/corplink-rs/blob/master/docs/reliability.md)。这些源文档不随二进制 Release 包分发。

[配置](#3-创建本机配置) · [启动](#4-启动) · [日常命令](#5-日常命令) · [验证访问](#6-验证访问) · [常见问题](#7-常见问题)

## 1. 准备依赖

先安装 macOS Command Line Tools：

```bash
xcode-select --install
```

还需要这些命令能在 shell 中使用：

```bash
git
python3
cargo
go
make
clang
sudo
```

如果本机还没有 Rust 或 Go，可以用自己习惯的方式安装，例如 Homebrew：

```bash
brew install rust go python
```

测试 GitHub 私有仓库访问时，需要提前配置好 GitHub SSH key。测试 Redshift 端口时，需要本机有 `nc`；macOS 默认通常已带。

## 2. Clone 项目并构建

把 `<repo-url>` 换成当前 fork 的地址：

```bash
git clone --recurse-submodules <repo-url> corplink-rs
cd corplink-rs
```

先构建 `libwg`：

```bash
cd libwg
./build.sh
cd ..
```

构建脚本会从固定的 WireGuard Git source 生成临时副本，应用 `libwg/patches/` 后构建。它不会改动 submodule 工作树；补丁或构建失败时保留上一份库。`libwg/build.sh --test` 还会验证 Go 包和真实 C 导出的用户态资源生命周期。

再构建 release binary：

```bash
cargo build --release --locked
```

脚本优先使用 `CORPLINK_BIN`，否则依次查找 `target/release/corplink-rs`、Release 包根目录的 `corplink-rs`。找不到可执行文件时才自动构建。更新源码后应手动执行 `cargo build --release --locked`；已有 binary 不会因源码变化而自动重建。

macOS/Linux Release 压缩包也包含 `scripts/`。解压后把 `config.json` 复制为 `config.local.json` 并编辑；脚本会识别包根目录的 `corplink-rs`。需要指定其他构建产物时设置 `CORPLINK_BIN`，预检和运行会使用同一份 binary。

## 3. 创建本机配置

从模板复制一份本机配置：

```bash
cp config.template.json config.local.json
```

`config.template.json` 是可提交模板；`config.local.json` 是本机真实配置，已经被 `.gitignore` 忽略，不应该提交。

编辑 `config.local.json`，至少替换这些字段：

```json
{
  "company_name": "company code name",
  "username": "your_name",
  "password": "your_real_password",
  "platform": "feilian",
  "interface_name": "utun12345",
  "use_vpn_dns": false,
  "auto_setup_routes": true,
  "route_mode": "split",
  "vpn_select_strategy": "latency",
  "managed_routes": {
    "enabled": true,
    "stale_ttl_secs": 86400,
    "include_ipv6": false,
    "cache_file": ".run/managed-routes-cache.json",
    "sources": [
      {
        "name": "github",
        "type": "github_meta",
        "keys": ["web", "api", "git"]
      },
      {
        "name": "redshift-prod",
        "type": "dns_hosts",
        "hosts": ["your-cluster.region.redshift.amazonaws.com"],
        "port": 5439
      }
    ]
  }
}
```

字段说明：

- `company_name`：公司代码。
- `username`：自己的登录账号。
- `password`：自己的登录密码。
- `platform`：通常用 `feilian`；如果公司环境只允许 LDAP 登录，改成 `ldap`。
- `interface_name`：本机 TUN 网卡名，默认可以先用 `utun12345`。
- `managed_routes.sources`：需要走 VPN 出口的公网目标。模板默认包含 GitHub 和 Redshift。

`platform` 和 `password` 的关系：

- `platform: "feilian"`：`password` 可以填真实密码；客户端会在登录前自动转成 sha256。也支持直接填 64 位 sha256 hex。
- `platform: "ldap"`：`password` 必须填真实 LDAP 密码；客户端不会做 sha256。
- 不建议用 `lark` / `OIDC` 配合后台 `start` 做首次登录；需要扫码、邮箱验证码等交互排障时，用 `foreground`。

如果暂时不用 Redshift，删除 `redshift-prod` 这个 source。需要 Redshift 时，把 `hosts` 里的 placeholder 换成真实 cluster/workgroup endpoint，并保留 `port: 5439`。

不要把自己的 `config.local.json` 发给别人。下面这些字段和文件是本机状态或个人凭据，每个人都应该自己生成：

- `code`
- `state`
- `device_id` / `device_name`
- `public_key` / `private_key`
- `*_cookies.jsonl`
- `*_session.json` 及其备份
- `.run/`

不需要导入浏览器 cookie。程序会自己生成 `*_cookies.jsonl`，它是 `cookie_store` 写出的 JSONL cookie store：一行一条 cookie 记录，不是一个整体 JSON 文档，也不是需要手动编辑的配置文件。

运行时生成的设备信息、WireGuard key 和认证状态保存在配置文件旁的 `<interface>_session.json`，不会自动改写用户配置。新的 cookie 文件名包含配置身份；旧 JSONL 按迁移规则保留，损坏文件在恢复写入前留存私密副本。更换账号时不会复用另一个账号的 cookie。这些 sidecar、备份和旧日志也不要对外分享。

## 4. 启动

后台启动：

```bash
scripts/corplink-traffic.sh start
```

脚本会做这些事：

- 读取 `config.local.json`。
- 构建缺失的 release binary。
- 用同一 Rust binary 预检 `managed_routes`。
- macOS 在前台完成所需的 `sudo`，将后台监护交给 launchd。
- 核对当前进程身份、运行代次和握手；TUN 模式也检查目标路由。
- 把脱敏状态、退出原因与日志写到 `.run/`，重启时追加日志。

system domain 使用的 plist 会通过 `sudo install` 发布为 `root:wheel`、`0644`。普通用户只写 plist 临时草稿；supervisor 登记进程身份，Rust 子进程发布握手就绪事实，启动命令等待这些事实。`launchctl bootout` 返回后，脚本仍会等待实际进程退出，因此不会把异步退出误报成停止失败。服务退出后再次 `start`，会先清理确认属于当前 checkout 和配置、且已停止的 launchd 注册。

`start` 在当前进程完成 WireGuard 握手、TUN 模式的探测目标路由匹配后返回 `ready`，随后后台监护继续工作。暂时故障会有限重试；本地资源清理失败、认证需要人工处理、不可恢复错误或重试耗尽会保留失败状态。macOS 默认对需要处理的后台失败发送本机通知；可以用 `CORPLINK_NOTIFY=0 scripts/corplink-traffic.sh start` 关闭后台通知。

如果前台等待超时，命令会说明后台仍在处理，并返回非零。此时用 `status` 查看进度；再次 `start` 会识别已有进程。`stop` 只有确认进程退出后才报告完成；失败时保留身份和诊断，`restart` 不会越过失败的停止步骤。

首次登录如果需要交互，改用 foreground：

```bash
scripts/corplink-traffic.sh foreground
```

前台会话也登记运行身份和状态，可从另一终端执行 `status` 或 `stop`。已有前台会话时，`start` 不会重复启动；按 Ctrl-C 会等待清理完成后退出。

前台退出后需要继续后台使用时，再执行 `scripts/corplink-traffic.sh start`。这两种方式管理的是同一个运行位置中的会话。

## 5. 日常命令

```bash
scripts/corplink-traffic.sh status      # 查看当前运行身份、健康与上次已应用路由
scripts/corplink-traffic.sh restart     # 修改 config.local.json 后重启刷新路由
scripts/corplink-traffic.sh logs        # 查看最近日志
scripts/corplink-traffic.sh logs -f     # 跟随日志
scripts/corplink-traffic.sh stop        # 停止当前 VPN 会话
```

`status` 仅在当前进程身份和健康观测有效时返回 0；停止、失效或观测过期时返回非零。它不再通过重新访问远端来猜测已应用路由；实时目标访问用 `test-host` 检查。

常用环境变量：

| 变量 | 作用 |
| --- | --- |
| `CORPLINK_CONFIG` | 指定配置文件，默认仓库或 Release 包根目录的 `config.local.json` |
| `CORPLINK_BIN` | 指定运行与预检使用的同一份可执行文件 |
| `CORPLINK_RUN_DIR` | 指定监护状态与日志目录，默认根目录的 `.run/`；不改变配置中的 managed routes cache 路径 |
| `CORPLINK_NOTIFY=0` | 启动时关闭 macOS 后台失败通知 |
| `TEST_HOST` / `TEST_PORT` | 覆盖探测目标与可选 TCP 端口，不修改 VPN 路由配置 |
| `TEST_REPO` | `test` 使用的 Git 仓库地址 |

对同一会话执行 `start`、`foreground`、`status`、`stop` 时，应使用同一组配置、binary 和运行目录。这些路径型环境变量中的相对路径按调用时的工作目录解析。同一个 TUN 接口不要并行启动多份配置。

`scripts/corplink-github.sh` 是兼容入口，转发到同一套命令并把默认 `TEST_HOST` 设为 `github.com`。

只检查 managed routes 解析结果：

```bash
scripts/update-managed-routes.py config.local.json --dry-run
```

`--dry-run` 只打印解析结果，不写 cache；需要预先刷新时将它换成 `--write-cache`，两个参数不能同时使用。默认 cache 为配置文件目录下的 `.run/managed-routes-cache.json`，可由 `managed_routes.cache_file` 覆盖。

Rust 也提供相同入口：

```bash
target/release/corplink-rs routes config.local.json
target/release/corplink-rs routes config.local.json --write-cache
target/release/corplink-rs routes-status config.local.json
```

预检不会登录、提权、生成 key 或写认证 sidecar。结果区分 `fresh` 和 `cache`。`routes-status` 返回 `last_applied` 历史记录，其中系统路由与 AllowedIPs 分列；是否仍在运行还须看当前运行状态。Netstack 模式不安装系统路由。

旧的 `scripts/update-github-extra-allowed-ips.py` 也转发到这个入口，不再写回 `extra_allowed_ips`。

## 6. 验证访问

检查默认目标路由：

```bash
scripts/corplink-traffic.sh test-host
```

默认优先检查 GitHub source，否则检查首个 `dns_hosts` host。只运行 `test-host` 是路由检查；要验证 TCP 连通性需提供 `TEST_PORT`，例如：

```bash
TEST_HOST=github.com TEST_PORT=443 scripts/corplink-traffic.sh test-host
```

测试 Redshift 端口。`dns_hosts` source 已经配置 `port: 5439` 时，不需要再写 host：

```bash
TEST_PORT=5439 scripts/corplink-traffic.sh test-host
```

测试 GitHub repo 访问时必须显式传入要检查的仓库；脚本不内置默认仓库：

```bash
TEST_REPO=git@github.com:owner/repo.git scripts/corplink-traffic.sh test
```

`TEST_PORT` 会优先选择 `port` 相同的 `dns_hosts` source。source 的 `port` 是选择探测目标的提示，不是端口转发或防火墙规则；显式 `TEST_HOST` 的优先级最高：

```bash
TEST_HOST=other.example.com TEST_PORT=443 scripts/corplink-traffic.sh test-host
```

## 7. 常见问题

如果 `start` 失败，先查看状态和日志：

```bash
scripts/corplink-traffic.sh status
scripts/corplink-traffic.sh logs -f
```

按结果处理：

| 结果 | 下一步 |
| --- | --- |
| `already running` | 已有就绪实例，可直接使用；重复 `start` 不会重建连接 |
| `already supervised` 或前台等待超时 | 后台仍可能连接中，先看 `status` 和日志 |
| 需要认证或交互 | 当前会话结束后运行 `foreground` 完成登录 |
| 清理失败或停止超时 | 保留状态与日志，先处理具体失败；`restart` 不会越过失败的 `stop` |
| 状态 JSON 损坏 | 保留原文件排查；监护不会通过反复解析来掩盖损坏 |

操作锁文件在会话结束后仍可存在，它不代表进程存活。旧目录锁会在核实原进程已退出后回收，无需手动删除 `.run/` 或 PID 文件。

如果路由没有走 VPN，用 `test-host` 检查当前目标解析到的 IP 和接口：

```bash
TEST_HOST=github.com scripts/corplink-traffic.sh test-host
```

如果改了 Redshift endpoint 或 GitHub source，重启进程刷新 managed routes：

```bash
scripts/corplink-traffic.sh restart
```

`managed_routes` 的结果会写入 `.run/managed-routes-cache.json`。当某个 source 临时解析失败时，未超过 `stale_ttl_secs` 且 source 输入完全匹配的旧结果会继续使用；首次启动且没有可用 cache 时会失败并指出具体 source。

当前 `wg-corplink` 的系统 route UAPI 只支持添加 route，不支持删除旧 route。因此目标 IP 变化后需要 `restart`，不要只改配置文件。

## 8. DNS 恢复与平台范围

### 与 Surge 增强模式共存

需要同时使用 Surge 与公司 VPN 时，可将 Corplink 作为本机 SOCKS 上游，由 Surge 按公司域名分流：

```json
{
  "socks5_listen": "127.0.0.1:1088",
  "route_mode": "full"
}
```

此处 `full` 仅作用于用户态隧道，不替换系统默认路由；只有交给该 SOCKS 入口的请求经过 VPN。监听地址保持 loopback。Surge 的策略和规则示例：

```ini
[Proxy]
Corplink = socks5, 127.0.0.1, 1088

[Rule]
# 公司域名规则应位于通用代理规则之前。
DOMAIN-SUFFIX,github.com,Corplink
DOMAIN-SUFFIX,company.example.com,Corplink
```

按实际需要补充公司域名和数据库 endpoint；VPN 节点的外层连接应单独直连，不能再次送入 Corplink。普通 `DIRECT` 不等于“经过公司 VPN”，验收必须包含 Surge 开启时的公司资源请求和普通上网请求。调整节点、TCP/UDP 或分流后，握手成功仍不能替代 HTTP/数据库访问验证。

### 平台行为

启用 `use_vpn_dns` 时，macOS 会在修改任何 DNS 前把原设置写入 `/var/run/corplink-rs/dns-backup.json`。成功恢复后才删除快照；异常退出后的下一次连接先处理遗留快照。快照损坏、归属不明或仍属于另一活实例时会拒绝覆盖，需保留文件排查。`dns_backup_filename` 可指定其他位置；macOS 相对路径以配置文件目录为准。

Linux 使用 `/etc/resolv.conf` 旁的备份；备份失败不会覆盖 resolver，恢复失败保留证据。持续后台运行可使用 [systemd 样例](https://github.com/Perfecto23/corplink-rs/blob/master/systemd/corplink-rs.service)：binary 位于 `/usr/bin/corplink-rs`，配置位于 `/etc/corplink/config.json`，实例化样例读取 `/etc/corplink/<实例名>.json`。程序负责连接恢复，systemd 对异常退出限次重启，应用已处理的失败退出码 `1` 不自动重启。Shell 的 Linux 后台模式只有进程 supervisor，不等同于 systemd 服务安装。

Windows Release 包中的 `setup.ps1` 下载 amd64 `wintun.dll` 到脚本所在目录；在解压后的包目录运行它，使 DLL 与 `corplink-rs.exe` 同目录，再从管理员终端启动：

```powershell
.\setup.ps1
.\corplink-rs.exe config.json
```

源码树中的安装脚本位于 `scripts/setup.ps1`，输出 DLL 也会放在 `scripts/`，需另行放到 executable 旁。Windows 使用原生 binary，不使用 macOS 的 launchd 或 Shell 状态监护。本机失败通知仅在 macOS 提供。

企业网关连接保留自签名证书兼容策略，客户端接受无效服务端证书；使用前应确认企业环境的信任要求。

## 9. 开发验证

```bash
libwg/build.sh --test
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
bash -n scripts/corplink-traffic.sh
python3 -m py_compile scripts/*.py
cargo build --release --locked
```

自动化测试使用回环网关、临时文件和受控操作系统命令。Go FFI probe 只创建用户态 netstack 和回环监听器，不创建系统 TUN 或连接真实 VPN。真实安装、断网/休眠恢复及各平台验证需要单独验收。

Windows 构建入口与 CI 一致：

```powershell
cd libwg
.\build.ps1 --test
cd ..
cargo build --release --locked
```

两个构建脚本都在应用补丁后的临时源码目录运行 Go 测试和构建。当前 [CI 定义](https://github.com/Perfecto23/corplink-rs/blob/master/.github/workflows/test.yml) 在 macOS/Linux 执行 Rust 与 CLI 测试，在 Windows 执行 PowerShell/Go 测试与 release 构建。

源码 `59e2000` 的本地验收覆盖了 macOS 前后台启动、跨终端停止、Ctrl-C、重复启动互斥、真实 sudo 和严格 `umask` 下的状态读取；PowerShell 入口已在 macOS 临时环境实跑。原生 Linux/Windows、休眠或真实断网恢复、桌面通知展示仍需对应环境验收，不能由 CI 配置或本机单元测试推定通过。
