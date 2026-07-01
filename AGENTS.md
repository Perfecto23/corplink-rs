# Repository Guidelines

## 项目结构与模块组织

这是一个 Rust CLI 项目，主入口在 `src/main.rs`。核心模块包括 `src/client.rs`（登录、VPN 选择与连接流程）、`src/wg.rs`（WireGuard/UAPI 集成）、`src/dns.rs`（DNS 配置）、`src/config.rs`（配置模型）、`src/managed_routes.rs`（GitHub/Redshift 等公网白名单路由解析）。测试主要以内联 `#[cfg(test)]` 单元测试放在对应模块中。

`libwg/` 包含 vendored `wireguard-go` 与本地构建脚本；`config.template.json` 是可提交配置模板，`config.local.json` 是被忽略的本机真实配置；`scripts/` 放启动、验证和 managed routes 辅助脚本；`systemd/` 放 Linux service 示例；`pack/` 放打包元数据。

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

## 提交与 PR 规范

历史提交主要使用 Conventional Commit 风格，例如 `feat: ...`、`fix: ...`、`feat(dns): ...`。提交信息应说明行为变化，不只描述文件变化。PR 需要包含改动摘要、验证命令输出、配置/安全影响；涉及平台行为时说明 macOS/Linux/Windows 覆盖范围。

## 安全与配置提示

不要提交 `config.local*.json`、cookie、`.run/` cache、`target/`、`libwg/libwg.a`、`libwg/libwg.h`。真实账号、密码、TOTP secret、WireGuard key、公司 endpoint 只应存在本机配置或安全 secret 管理中。示例配置使用 placeholder。
