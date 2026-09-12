# Repository Guidelines

## 项目结构与模块组织

这是一个 Rust CLI 项目。使用与配置见 [README](README.md)，术语见 [CONTEXT](CONTEXT.md)。涉及运行状态、恢复、资源收尾、持久化或平台适配时，先读 [可靠性合同](docs/reliability.md)。

`src/main.rs` 编排连接和 `supervise_session`；`src/runtime.rs` 管理运行状态与恢复策略；`src/client.rs` 处理认证、选线和远端协商；`src/network_session.rs` 负责本地资源生命周期；`src/wg.rs`、`src/dns.rs` 是平台适配；`src/config.rs` 与 `src/state.rs` 分别处理用户配置和认证会话持久化；`src/managed_routes.rs` 是路由解析、缓存和已应用记录的真源。

`libwg/` 包含固定版本的 `wireguard-go` submodule、补丁与构建脚本；`config.template.json` 是可提交配置模板，`config.local.json` 是被忽略的本机真实配置；`scripts/` 放操作入口与兼容脚本；`systemd/` 放 Linux service 示例；`pack/` 放打包元数据。Rust 测试以内联 `#[cfg(test)]` 为主，CLI 和 C ABI 验证位于 `tests/`。

## Agent 快速上下文

`scripts/corplink-traffic.sh` 是 macOS 日常入口；`scripts/corplink-github.sh` 转发同一套命令。预检走 Rust `routes`，历史应用记录走 `routes-status`，实时路由和 TCP 探测走 `test-host`。Python 兼容入口不另写解析规则。

快速跑通的最短路径是：

```bash
libwg/build.sh
cargo build --release --locked
cp config.template.json config.local.json
$EDITOR config.local.json
scripts/update-managed-routes.py config.local.json --dry-run
scripts/corplink-traffic.sh start
```

`start`、`foreground`、`restart`、`stop` 会触发真实 VPN 进程和 `sudo`，须在本机运行授权范围内执行。普通代码验证使用隔离测试；文档命令使用临时配置。自动化成功与实机成功分别报告，不能以假 sudo 或进程存在代替真实启动验收。

配置格式以 `src/config.rs` 为准，示例见模板和 README。用户配置只读加载；运行生成的设备信息、key 与认证状态写入独立 sidecar。兼容读取旧字段时保持账号隔离，不恢复完整配置自动回写。

`*_cookies.jsonl` 由 `cookie_store` crate 自动生成，实际是 JSONL cookie store，一行一条 cookie 记录，不是整体 JSON 配置文件，也不是浏览器 cookie 导出文件。不要手动编辑、格式化或复制给别人。

## 构建、测试与本地运行命令

- `libwg/build.sh --test`：在临时 patched source 中执行 Go/C ABI 验证并构建库，保留 submodule 工作树。
- Windows：在 `libwg/` 执行 `./build.ps1 --test`，Go 测试和构建同样在 patched source 中运行。
- `cargo build`：开发构建。
- `cargo build --release --locked`：生成 release binary；改源码后显式重建，启动脚本只会在缺少 binary 时自动构建。
- `cargo test --locked`：运行 Rust 测试。
- `python3 -m unittest discover -s tests -p 'test_*.py'`：验证操作脚本和 Release 包入口。
- `scripts/update-managed-routes.py config.local.json --dry-run`：安全解析 managed routes，不打印主配置 secrets。
- `scripts/corplink-traffic.sh start|foreground|restart|status|preflight|test|test-host|logs|stop`：本机启动与路由验证入口。

## 代码风格与命名约定

Rust 代码使用 `cargo fmt` 默认格式；保持 4 空格缩进。模块、函数、变量使用 `snake_case`，类型和 enum 使用 `PascalCase`。错误处理优先使用 `anyhow::{Context, Result}`，给外部 IO、配置解析、网络请求补充上下文。注释只解释非显而易见的边界、失败语义或平台差异，避免复述代码。

Shell 脚本使用 `set -euo pipefail`。Python 脚本保持标准库优先，避免引入额外运行依赖。

## 测试规范

新增行为在对应 module 或 CLI interface 补局部回归。恢复测试要经过生产的握手、监测、清理和重试决策，覆盖失败结果与正常结果。CLI 替身需保留影响行为的 OS 合同，例如 sudo 的文件描述符处理、异步退出和严格 umask。代码提交前至少运行：

```bash
cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
python3 -m py_compile scripts/*.py tests/*.py
bash -n scripts/corplink-traffic.sh
bash -n scripts/corplink-github.sh
```

涉及 Go 桥时再运行 `libwg/build.sh --test` 或 Windows 对应入口。有本机运行授权时，启动脚本改动还需核对真实启动/停止结果，用 `status` 和明确目标的 `test-host` 读回。仅改文档或帮助文案时，检查链接、代表性命令和相应脚本语法即可。

## 文档与分发一致性

迁移、删除或重命名示例配置和模板文件时，同步检查 `.github/`、`pack/`、`systemd/`、`scripts/`、README 的消费者。源码模板是 `config.template.json`，Release 包内为 `config.json`，Shell 操作默认读取用户创建的 `config.local.json`，三者用途不同。

README 中描述脚本环境变量、自动推导或测试命令时，必须用最小临时配置验证代表性命令。涉及 `managed_routes` 时，确认文档里的 `TEST_HOST`、`TEST_PORT`、`dns_hosts.port` 与脚本实际选择目标一致。

## 提交与 PR 规范

历史提交主要使用 Conventional Commit 风格，例如 `feat: ...`、`fix: ...`、`feat(dns): ...`。提交信息应说明行为变化，不只描述文件变化。PR 需要包含改动摘要、验证命令输出、配置/安全影响；涉及平台行为时说明 macOS/Linux/Windows 覆盖范围。

## 安全与配置提示

不要提交 `config.local*.json`、认证 sidecar、cookie 及其备份、`.run/`、`target/`、`libwg/libwg.a`、`libwg/libwg.h`。真实账号、密码、TOTP secret、WireGuard key、公司 endpoint 只应存在本机配置或安全 secret 管理中。示例配置使用 placeholder。Unix 新写入的认证 sidecar、cookie 及备份使用 `0600`；不含认证秘密的运行状态使用 `0644`，保证普通用户能读取 root 进程的诊断结果。
