# Repository Guidelines

## 项目结构与模块组织

这是一个 Rust CLI 项目，主入口在 `src/main.rs`。核心模块包括 `src/client.rs`（登录、VPN 选择与连接流程）、`src/wg.rs`（WireGuard/UAPI 集成）、`src/dns.rs`（DNS 配置）、`src/config.rs`（配置模型）、`src/managed_routes.rs`（GitHub/Redshift 等公网白名单路由解析）。测试主要以内联 `#[cfg(test)]` 单元测试放在对应模块中。

`libwg/` 包含 vendored `wireguard-go` 与本地构建脚本；`config.template.json` 是可提交配置模板，`config.local.json` 是被忽略的本机真实配置；`scripts/` 放启动、验证和 managed routes 辅助脚本；`systemd/` 放 Linux service 示例；`pack/` 放打包元数据。

## Agent 快速上下文

优先阅读这些核心代码：`src/main.rs` 负责加载配置并进入客户端流程；`src/config.rs` 定义配置 schema；`src/client.rs` 负责登录、VPN 选择、AllowedIPs 合并和启动 WireGuard；`src/managed_routes.rs` 解析 GitHub Meta / DNS host 白名单路由并处理 cache fallback；`src/utils.rs` 放 CIDR/route 工具；`scripts/corplink-traffic.sh` 是 macOS 本地启动与验证入口。

快速跑通的最短路径是：

```bash
cd libwg && ./build.sh && cd ..
cargo build --release
cp config.template.json config.local.json
$EDITOR config.local.json
scripts/update-managed-routes.py config.local.json --dry-run
scripts/corplink-traffic.sh start
```

`start`、`foreground`、`restart`、`stop` 会触发真实 VPN 进程和 `sudo`，只有在用户明确要求运行本机 VPN 时执行。普通代码/文档验证优先使用 `cargo test`、`cargo build --release`、`bash -n scripts/corplink-traffic.sh`、`python3 -m py_compile scripts/*.py` 和 `scripts/update-managed-routes.py ... --dry-run`。

必须配置项包括 `company_name`、`username`、`password`、`platform`、`interface_name`。要让 GitHub/Redshift 这类白名单目标走 VPN，必须配置 `managed_routes.enabled=true` 和至少一个 `sources`：GitHub 用 `type: "github_meta"`；Redshift 用 `type: "dns_hosts"`、`hosts` 和 `port: 5439`。真实 endpoint、账号密码、TOTP、cookie、WireGuard key 只应出现在本机 `config.local.json` 或本地状态文件中。

`*_cookies.jsonl` 由 `cookie_store` crate 自动生成，实际是 JSONL cookie store，一行一条 cookie 记录，不是整体 JSON 配置文件，也不是浏览器 cookie 导出文件。不要手动编辑、格式化或复制给别人。

## 构建、测试与本地运行命令

- `cd libwg && ./build.sh && cd ..`：首次构建本地 `libwg` 静态库和头文件。
- `cargo build`：开发构建。
- `cargo build --release`：生成 `target/release/corplink-rs`，启动脚本默认使用它。
- `cargo test`：运行 Rust 单元测试。
- `scripts/update-managed-routes.py config.local.json --dry-run`：安全解析 managed routes，不打印主配置 secrets。
- `scripts/corplink-traffic.sh start|foreground|restart|status|preflight|test|test-host|logs|stop`：本机启动与路由验证入口。

## 代码风格与命名约定

Rust 代码使用 `cargo fmt` 默认格式；保持 4 空格缩进。模块、函数、变量使用 `snake_case`，类型和 enum 使用 `PascalCase`。错误处理优先使用 `anyhow::{Context, Result}`，给外部 IO、配置解析、网络请求补充上下文。注释只解释非显而易见的边界、失败语义或平台差异，避免复述代码。

Shell 脚本使用 `set -euo pipefail`。Python 脚本保持标准库优先，避免引入额外运行依赖。

## 测试规范

新增行为必须补局部单元测试，优先放在对应 `src/*.rs` 的 `tests` 模块中。路由、CIDR、DNS、cache fallback 这类边界逻辑需要覆盖失败路径和去重/规范化行为。提交前至少运行：

```bash
cargo test
python3 -m py_compile scripts/*.py
bash -n scripts/corplink-traffic.sh
```

涉及启动脚本或系统路由时，再用 `scripts/corplink-traffic.sh status` 和 `test-host` 做本机验证。

## 文档与分发一致性

迁移、删除或重命名示例配置和模板文件时，必须同步检查 `.github/`、`pack/`、`systemd/`、`scripts/`、README 中的旧路径引用。例如 `config/config.json` 改为 `config.template.json` 时，release workflow 仍应把模板打包成产物内的 `config.json`。

README 中描述脚本环境变量、自动推导或测试命令时，必须用最小临时配置验证代表性命令。涉及 `managed_routes` 时，确认文档里的 `TEST_HOST`、`TEST_PORT`、`dns_hosts.port` 与脚本实际选择目标一致。

## 提交与 PR 规范

历史提交主要使用 Conventional Commit 风格，例如 `feat: ...`、`fix: ...`、`feat(dns): ...`。提交信息应说明行为变化，不只描述文件变化。PR 需要包含改动摘要、验证命令输出、配置/安全影响；涉及平台行为时说明 macOS/Linux/Windows 覆盖范围。

## 安全与配置提示

不要提交 `config.local*.json`、cookie、`.run/` cache、`target/`、`libwg/libwg.a`、`libwg/libwg.h`。真实账号、密码、TOTP secret、WireGuard key、公司 endpoint 只应存在本机配置或安全 secret 管理中。示例配置使用 placeholder。
