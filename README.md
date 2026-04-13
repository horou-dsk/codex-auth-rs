# codex-auth-rs

`codex-auth-rs` 是将 `codex-auth` Zig 项目重写为 Rust 的命令行工具，用于管理和切换 Codex 账号。

当前项目已经具备可编译、可运行的基础账号管理能力，并兼容本地 `~/.codex` 目录结构。

## 安装

使用 Cargo 直接从 GitHub 安装：

```bash
cargo install --git https://github.com/horou-dsk/codex-auth-rs.git
```

## 当前能力

- 读取并解析 `auth.json`
- 管理 `accounts/registry.json`
- 保存每个账号的快照 `accounts/*.auth.json`
- 支持 `list` / `login` / `import` / `switch` / `remove` / `status` / `clean` / `config` / `daemon`
- 支持从本地 `sessions/rollout-*.jsonl` 刷新 usage
- 支持通过 ChatGPT API 刷新 usage 和 account name
- 支持按邮箱分组展示账号列表
- 支持基于 usage 阈值的基础自动切换

## 当前未完全对齐 Zig 版的部分

- 尚未实现 Zig 版完整的平台托管服务安装逻辑
  - Windows Scheduled Task
  - macOS LaunchAgent
  - Linux systemd user service
- 交互输出和 TUI 细节仍是简化版
- 测试覆盖还未迁移完整

## 构建

需要 Rust stable。

```bash
cargo build
```

二进制名称是：

```bash
codex-auth
```

直接运行：

```bash
cargo run -- <subcommand>
```

例如：

```bash
cargo run -- list
cargo run -- status
```

## 命令

### 列出账号

```bash
cargo run -- list
```

### 登录并添加当前账号

```bash
cargo run -- login
cargo run -- login --device-auth
```

Windows 下会通过 PowerShell 调 `codex login`，以兼容 `codex.ps1`。

### 导入账号

标准 auth 文件：

```bash
cargo run -- import /path/to/auth.json
cargo run -- import /path/to/folder --alias work
```

导入 CPA JSON：

```bash
cargo run -- import --cpa
cargo run -- import --cpa /path/to/cpa-dir
cargo run -- import --cpa /path/to/token.json --alias work
```

重建 registry：

```bash
cargo run -- import --purge
cargo run -- import --purge /path/to/accounts
```

### 切换账号

```bash
cargo run -- switch
cargo run -- switch john
cargo run -- switch work
```

### 删除账号

```bash
cargo run -- remove
cargo run -- remove john
cargo run -- remove --all
```

### 查看状态

```bash
cargo run -- status
```

### 自动切换配置

启用或关闭：

```bash
cargo run -- config auto enable
cargo run -- config auto disable
```

修改阈值：

```bash
cargo run -- config auto --5h 12
cargo run -- config auto --weekly 8
cargo run -- config auto --5h 12 --weekly 8
```

### API 刷新配置

默认启用。

关闭后：

- 不再调用 ChatGPT usage API
- 不再刷新 account/team name
- usage 仅从本地 `sessions/*.jsonl` 读取

命令：

```bash
cargo run -- config api disable
cargo run -- config api enable
```

### 守护模式

单次执行：

```bash
cargo run -- daemon --once
```

持续轮询：

```bash
cargo run -- daemon --watch
```

## 数据目录

默认使用：

```text
~/.codex
```

关键文件：

- `auth.json`
- `accounts/registry.json`
- `accounts/*.auth.json`
- `sessions/rollout-*.jsonl`

也支持通过环境变量覆盖：

```bash
CODEX_HOME=/custom/codex-home
```

Windows PowerShell：

```powershell
$env:CODEX_HOME="D:\path\to\codex-home"
```

## API 刷新说明

当前实现会在 `api.usage=true` 时优先尝试通过 ChatGPT API 刷新 active account 的 usage。

如果 API 请求失败，则回退到本地 `sessions` 扫描。

当前 API 使用：

- `https://chatgpt.com/backend-api/wham/usage`
- `https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27`

请求头会包含：

- `Authorization: Bearer <access_token>`
- `ChatGPT-Account-Id: <account_id>`

## Windows 说明

如果你在 Windows 下执行 `codex-auth login` 报错，而 `Get-Command codex` 返回的是 `codex.ps1`，当前 Rust 版已经做了兼容处理，会通过 PowerShell 调用 `codex login`。

## 开发

检查编译：

```bash
cargo check
```

## 项目状态

这是一个正在持续补齐 Zig 原版能力的 Rust 重写项目。当前已经可用于本地账号管理和基础自动切换，但还没有完全达到 Zig 版的功能完备度。
