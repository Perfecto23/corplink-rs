# corplink-rs macOS 使用说明

这个仓库用于在 macOS 上启动 `corplink-rs`，并把 GitHub、Redshift 等有 IP 白名单限制的公网目标通过公司 VPN 出口访问。

日常只需要维护本机的 `config.local.json`，然后运行：

```bash
scripts/corplink-traffic.sh start
```

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
git clone <repo-url> corplink-rs
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
cargo build --release
```

`scripts/corplink-traffic.sh start` 默认会使用 `target/release/corplink-rs`。如果这个文件不存在，脚本会自动执行 `cargo build --release`。

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

`start` 返回 ready 后，后台监护继续工作。暂时故障会有限重试；认证需要人工处理、不可恢复错误或重试耗尽会保留失败状态。macOS 默认对需要处理的后台失败发送本机通知；可以用 `CORPLINK_NOTIFY=0 scripts/corplink-traffic.sh start` 关闭。

如果前台等待超时，命令会说明后台仍在处理，并返回非零。此时用 `status` 查看进度；再次 `start` 会识别已有进程。`stop` 只有确认进程退出后才报告完成；失败时保留身份和诊断，`restart` 不会越过失败的停止步骤。

首次登录如果需要交互，改用 foreground：

```bash
scripts/corplink-traffic.sh foreground
```

## 5. 日常命令

```bash
scripts/corplink-traffic.sh status      # 查看当前运行身份、健康与上次已应用路由
scripts/corplink-traffic.sh restart     # 修改 config.local.json 后重启刷新路由
scripts/corplink-traffic.sh logs        # 查看最近日志
scripts/corplink-traffic.sh logs -f     # 跟随日志
scripts/corplink-traffic.sh stop        # 停止后台进程
```

`status` 仅在当前进程身份和健康观测有效时返回 0；停止、失效或观测过期时返回非零。它不再通过重新访问远端来猜测已应用路由；实时目标访问用 `test-host` 检查。

只检查 managed routes 解析结果：

```bash
scripts/update-managed-routes.py config.local.json --dry-run
```

`--dry-run` 只打印解析结果，不写 `.run/managed-routes-cache.json`；需要预先刷新 cache 时显式加 `--write-cache`。

Rust 也提供相同入口：

```bash
target/release/corplink-rs routes config.local.json
target/release/corplink-rs routes config.local.json --write-cache
target/release/corplink-rs routes-status config.local.json
```

预检不会登录、提权、生成 key 或写认证 sidecar。结果区分 `fresh` 和 `cache`。`routes-status` 返回 `last_applied` 历史记录，其中系统路由与 AllowedIPs 分列；是否仍在运行还须看当前运行状态。Netstack 模式不安装系统路由。

## 6. 验证访问

检查默认目标路由：

```bash
scripts/corplink-traffic.sh test-host
```

测试 Redshift 端口。`dns_hosts` source 已经配置 `port: 5439` 时，不需要再写 host：

```bash
TEST_PORT=5439 scripts/corplink-traffic.sh test-host
```

测试 GitHub repo 访问时必须显式传入要检查的仓库；脚本不内置默认仓库：

```bash
TEST_REPO=git@github.com:owner/repo.git scripts/corplink-traffic.sh test
```

`TEST_HOST` 不是配置项，只是临时排查用的覆盖值。只有当你想临时检查一个没有写进 `managed_routes` 的目标时才需要它：

```bash
TEST_HOST=other.example.com TEST_PORT=443 scripts/corplink-traffic.sh test-host
```

## 7. 常见问题

如果 `start` 失败，先看日志：

```bash
scripts/corplink-traffic.sh logs -f
```

如果路由没有走 VPN，检查当前目标解析到了哪些 IP、走哪个接口：

```bash
scripts/corplink-traffic.sh status
```

如果改了 Redshift endpoint 或 GitHub source，重启进程刷新 managed routes：

```bash
scripts/corplink-traffic.sh restart
```

`managed_routes` 的结果会写入 `.run/managed-routes-cache.json`。当某个 source 临时解析失败时，未超过 `stale_ttl_secs` 且 source 输入完全匹配的旧结果会继续使用；首次启动且没有可用 cache 时会失败并指出具体 source。

当前 `wg-corplink` 的系统 route UAPI 只支持添加 route，不支持删除旧 route。因此目标 IP 变化后需要 `restart`，不要只改配置文件。

## 8. DNS 恢复与平台范围

启用 `use_vpn_dns` 时，macOS 会在修改任何 DNS 前把原设置写入 `/var/run/corplink-rs/dns-backup.json`。成功恢复后才删除快照；异常退出后的下一次连接先处理遗留快照。快照损坏、归属不明或仍属于另一活实例时会拒绝覆盖，需保留文件排查。`dns_backup_filename` 可指定其他位置；macOS 相对路径以配置文件目录为准。

Linux 继续使用 `/etc/resolv.conf` 旁的备份；备份失败不会覆盖 resolver，恢复失败保留证据。`systemd/` 样例由程序处理正常连接重试，systemd 负责有上限的异常进程重启。macOS 日常入口使用 launchd；Windows 保留原有 binary/setup 入口，本机通知功能仅在 macOS 提供。

当前仍保留原有自签名证书兼容策略，不验证服务端证书链；本轮没有静默切换企业证书信任配置。

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
