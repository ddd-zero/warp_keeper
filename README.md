# warp-keeper

`warp-keeper` 是 Linux/Unix 下的 WARP 保活工具：通过 WARP 网卡执行可配置主检测（`ping`/`tcp`/`http`），断线后执行重连命令，再执行重连校验。

## 支持范围

- 仅提供 Linux/Unix 兼容环境支持
- 发布产物为 `musl` 静态链接二进制
- x86_64 提供两套优化包：
  - 基础兼容版（不启用 AVX2）：`warp-keeper-linux-x86_64-musl`
  - AVX2 优化版（`x86-64-v3`）：`warp-keeper-linux-x86_64-musl-avx2`

## 客户端识别规则



- 识别命令为内置逻辑，不开放配置
- `detect` 只做一次性识别：
  - 识别成功：把客户端写入 `reconnect.warp_client`，并写入对应 `reconnect.commands`
  - 识别失败：写入空的 `reconnect.warp_client = ""`，并清空 `reconnect.commands`

## 常用命令

```bash
warp-keeper detect --config ./config.toml
warp-keeper check --config ./config.toml
warp-keeper run --config ./config.toml
```

## 配置说明

- `general`：间隔、失败阈值、重连冷却、shell、日志等级、日志文件
- `reconnect`：当前识别到的客户端、重连命令列表
- `monitor.primary_check`：主检测方法（`ping` / `tcp` / `http` 三选一）
- `monitor.reconnect_verify`：重连后检测方法列表（可配置多个，必须全部成功）

## `config.toml` 示例（带注释）

```toml
[general]
# 主检测循环间隔（秒）
interval_secs = 2
# 连续失败多少次后触发重连
failure_threshold = 3
# 重连命令执行后额外等待（秒）
reconnect_cooldown_secs = 2
# 执行命令使用的 shell
shell = "/bin/bash"
# 日志等级: error/warn/info/debug
log_level = "info"
# 日志文件路径（若初始化失败，仅输出到终端）
log_file = "/var/log/warp-keeper.log"

[reconnect]
# detect 识别出的客户端；未识别时保持空字符串,值示例: warp-official / warp-wg / warp-go
warp_client = "warp-go"
# 重连命令，按顺序串行执行，前一步失败则中止
commands = ["warp-go o", "warp-go o"]

[monitor]
# 可选：手动指定网卡名；不填则自动匹配包含 warp 的网卡
# interface_name = "warp"
# 主检测方法: ping / tcp / http
primary_check = { method = "ping", target = "8.8.8.8", timeout_secs = 1 }
# 重连后检测列表：可配多个，必须全部成功
reconnect_verify = [
  # HTTP 检测仅支持 http://（不支持 https://，expect_contains关键字检测，大小写敏感，可不填）
  { method = "http", url = "http://www.apple.com/library/test/success.html", timeout_secs = 3, expect_status = 200, expect_contains = "Success" },
  # TCP 检测（等价 tcping）
  { method = "tcp", target = "1.1.1.1", port = 80, timeout_secs = 3 }
]
```

## musl 编译

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## 进程守护

发布包仅包含二进制文件，不再包含守护模板目录。安装脚本会按发布标签从仓库 `deploy/` 目录下载模板并注册守护：

- `deploy/config.toml`：首次安装时下载到 `/etc/warp-keeper/config.toml`，随后自动执行一次 `detect`
- `deploy/systemd/warp-keeper.service`：适用于 systemd 发行版（Debian/Ubuntu/CentOS 等）
- `deploy/openrc/warp-keeper`：适用于 OpenRC 发行版（Alpine/Gentoo 等）

安装脚本会自动识别并注册对应守护进程。

## 一键安装命令

```bash
# 1) 安装普通版本（baseline）
curl -fsSL https://raw.githubusercontent.com/ddd-zero/warp_keeper/main/deploy/install.sh | sudo bash -s -- --force-baseline

# 2) 安装 AVX2 最新版本
curl -fsSL https://raw.githubusercontent.com/ddd-zero/warp_keeper/main/deploy/install.sh | sudo bash -s -- --force-avx2

# 3) 安装 AVX2 指定版本
curl -fsSL https://raw.githubusercontent.com/ddd-zero/warp_keeper/main/deploy/install.sh | sudo bash -s -- --force-avx2 --tag v1.0.0
```

## 日志

- 支持 `error/warn/info/debug`
- 日志文件初始化失败时，不写入磁盘，仅输出到终端

## 许可证

Apache License 2.0
