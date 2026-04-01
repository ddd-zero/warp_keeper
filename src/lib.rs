use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WarpClient {
    WarpOfficial,
    WarpWg,
    WarpGo,
}

impl fmt::Display for WarpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::WarpOfficial => "warp-official",
            Self::WarpWg => "warp-wg",
            Self::WarpGo => "warp-go",
        };
        write!(f, "{text}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl LogLevel {
    fn priority(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warn => 1,
            Self::Info => 2,
            Self::Debug => 3,
        }
    }

    pub fn allows(self, msg_level: LogLevel) -> bool {
        msg_level.priority() <= self.priority()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub reconnect: ReconnectConfig,
    #[serde(default)]
    pub monitor: MonitorConfig,
    #[serde(default)]
    pub reconnect_verify: ReconnectVerifyConfig,
}

impl AppConfig {
    pub fn validate(&self) -> Result<()> {
        if self.general.interval_secs == 0 {
            return Err(anyhow!("`general.interval_secs` 必须大于 0"));
        }
        if self.general.failure_threshold == 0 {
            return Err(anyhow!("`general.failure_threshold` 必须大于 0"));
        }
        if self.general.shell.trim().is_empty() {
            return Err(anyhow!("`general.shell` 不能为空"));
        }
        if self.general.log_file.trim().is_empty() {
            return Err(anyhow!("`general.log_file` 不能为空"));
        }

        validate_check(&self.monitor.primary_check, "monitor.primary_check")?;

        for (idx, check) in self.reconnect_verify.checks.iter().enumerate() {
            validate_check(check, &format!("reconnect_verify.checks[{idx}]"))?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_reconnect_cooldown_secs")]
    pub reconnect_cooldown_secs: u64,
    #[serde(default = "default_shell")]
    pub shell: String,
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default = "default_log_file")]
    pub log_file: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_interval_secs(),
            failure_threshold: default_failure_threshold(),
            reconnect_cooldown_secs: default_reconnect_cooldown_secs(),
            shell: default_shell(),
            log_level: LogLevel::Info,
            log_file: default_log_file(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReconnectConfig {
    #[serde(default)]
    pub warp_client: Option<WarpClient>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    #[serde(default)]
    pub interface_name: Option<String>,
    #[serde(default = "default_primary_check")]
    pub primary_check: HealthCheck,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interface_name: None,
            primary_check: default_primary_check(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectVerifyConfig {
    #[serde(default = "default_reconnect_verify_checks")]
    pub checks: Vec<HealthCheck>,
}

impl Default for ReconnectVerifyConfig {
    fn default() -> Self {
        Self {
            checks: default_reconnect_verify_checks(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum HealthCheck {
    Ping {
        target: String,
        #[serde(default = "default_ping_timeout_secs")]
        timeout_secs: u64,
    },
    Tcp {
        target: String,
        port: u16,
        #[serde(default = "default_check_timeout_secs")]
        timeout_secs: u64,
    },
    Http {
        url: String,
        #[serde(default = "default_check_timeout_secs")]
        timeout_secs: u64,
        #[serde(default)]
        expect_status: Option<u16>,
        #[serde(default)]
        expect_contains: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct SingleCheckResult {
    pub name: String,
    pub success: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct CheckReport {
    pub all_success: bool,
    pub checks: Vec<SingleCheckResult>,
}

pub trait CommandProbe {
    fn command_ok(&self, command: &str) -> bool;
}

pub struct SystemCommandProbe {
    shell: String,
}

impl SystemCommandProbe {
    pub fn new(shell: String) -> Self {
        Self { shell }
    }
}

impl CommandProbe for SystemCommandProbe {
    fn command_ok(&self, command: &str) -> bool {
        run_shell_status(&self.shell, command)
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

pub struct Logger {
    level: LogLevel,
    file: Option<Mutex<File>>,
}

impl Logger {
    pub fn from_config(general: &GeneralConfig) -> Result<Self> {
        Self::new(general.log_level, &general.log_file)
    }

    pub fn console_only(level: LogLevel) -> Self {
        Self { level, file: None }
    }

    pub fn new(level: LogLevel, path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("创建日志目录失败: {}", parent.display()))?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("打开日志文件失败: {path}"))?;

        Ok(Self {
            level,
            file: Some(Mutex::new(file)),
        })
    }

    pub fn error(&self, message: &str) {
        self.log(LogLevel::Error, message);
    }

    pub fn warn(&self, message: &str) {
        self.log(LogLevel::Warn, message);
    }

    pub fn info(&self, message: &str) {
        self.log(LogLevel::Info, message);
    }

    pub fn debug(&self, message: &str) {
        self.log(LogLevel::Debug, message);
    }

    pub fn log(&self, level: LogLevel, message: &str) {
        if !self.level.allows(level) {
            return;
        }

        let line = format!(
            "[{}][{}] {}",
            now_unix_seconds(),
            level_name(level),
            message
        );
        if level == LogLevel::Error {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }

        if let Some(file) = &self.file
            && let Ok(mut file) = file.lock()
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

pub fn read_config(path: &Path) -> Result<AppConfig> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
    let config: AppConfig = toml::from_str(&content)
        .with_context(|| format!("解析配置文件失败: {}", path.display()))?;
    config.validate()?;
    Ok(config)
}

pub fn write_config(path: &Path, config: &AppConfig) -> Result<()> {
    config.validate()?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
    }

    let content = render_config(config)?;
    fs::write(path, content).with_context(|| format!("写入配置失败: {}", path.display()))?;
    Ok(())
}

pub fn load_or_create_config(path: &Path) -> Result<AppConfig> {
    if path.exists() {
        return read_config(path);
    }
    let config = AppConfig::default();
    write_config(path, &config)?;
    Ok(config)
}

pub fn init_config(path: &Path, force: bool) -> Result<AppConfig> {
    if path.exists() && !force {
        return Err(anyhow!(
            "配置文件已存在: {}（如需覆盖请使用 --force）",
            path.display()
        ));
    }
    let config = AppConfig::default();
    write_config(path, &config)?;
    Ok(config)
}

pub fn render_config(config: &AppConfig) -> Result<String> {
    let body = toml::to_string_pretty(config).context("序列化配置失败")?;
    let header = r#"# warp-keeper 配置文件
# 说明：
# 1) 客户端识别顺序固定：warp-official -> warp-wg -> warp-go
# 2) 识别命令内置，不开放配置修改
# 3) detect 会把识别到客户端对应的命令写入 reconnect.commands
# 4) 主检测与重连后检测都使用 method 抽象（ping/tcp/http）

"#;
    Ok(format!("{header}{body}"))
}

pub fn detect_client_builtin(probe: &dyn CommandProbe) -> Option<WarpClient> {
    if probe.command_ok("systemctl is-active --quiet warp-svc")
        || probe.command_ok("warp-cli --accept-tos status 2>/dev/null | grep -qi Connected")
    {
        return Some(WarpClient::WarpOfficial);
    }

    if probe.command_ok("command -v warp >/dev/null 2>&1")
        || probe.command_ok("systemctl is-active --quiet wg-quick@warp")
        || probe.command_ok("wg show warp >/dev/null 2>&1")
    {
        return Some(WarpClient::WarpWg);
    }

    if probe.command_ok("command -v warp-go >/dev/null 2>&1")
        || probe.command_ok("systemctl is-active --quiet warp-go")
        || probe.command_ok("pgrep -x warp-go >/dev/null 2>&1")
    {
        return Some(WarpClient::WarpGo);
    }

    None
}

pub fn detect_client_now(path: &Path, config: &mut AppConfig) -> Result<Option<WarpClient>> {
    let probe = SystemCommandProbe::new(config.general.shell.clone());
    let detected = detect_client_builtin(&probe);

    match detected {
        Some(client) => {
            config.reconnect.warp_client = Some(client);
            config.reconnect.commands = default_reconnect_commands(client);
        }
        None => {
            config.reconnect.warp_client = None;
            config.reconnect.commands.clear();
        }
    }

    write_config(path, config)?;
    Ok(detected)
}

pub fn find_warp_interface(config: &AppConfig) -> Result<Option<String>> {
    if let Some(name) = &config.monitor.interface_name
        && interface_exists(name)
    {
        return Ok(Some(name.clone()));
    }

    let mut names = list_interfaces()?;
    names.sort();

    if let Some(exact) = names.iter().find(|n| n.eq_ignore_ascii_case("warp")) {
        return Ok(Some(exact.to_string()));
    }

    if let Some(partial) = names
        .iter()
        .find(|n| n.to_ascii_lowercase().contains("warp"))
    {
        return Ok(Some(partial.to_string()));
    }

    Ok(None)
}

pub fn run_primary_check(config: &AppConfig, interface: &str) -> SingleCheckResult {
    run_health_check(&config.monitor.primary_check, interface)
}

pub fn execute_reconnect(config: &AppConfig) -> Result<()> {
    if config.reconnect.commands.is_empty() {
        return Err(anyhow!("`reconnect.commands` 为空，无法执行重连"));
    }

    for (idx, cmd) in config.reconnect.commands.iter().enumerate() {
        let status = run_shell_status(&config.general.shell, cmd)
            .with_context(|| format!("执行第 {} 条重连命令失败: `{cmd}`", idx + 1))?;
        if !status.success() {
            return Err(anyhow!("第 {} 条重连命令返回非 0 退出码: `{cmd}`", idx + 1));
        }
    }

    Ok(())
}

pub fn run_reconnect_verify_checks(config: &AppConfig, interface: &str) -> CheckReport {
    let checks = config
        .reconnect_verify
        .checks
        .iter()
        .map(|check| run_health_check(check, interface))
        .collect::<Vec<_>>();
    let all_success = checks.iter().all(|x| x.success);
    CheckReport {
        all_success,
        checks,
    }
}

pub fn run_shell_status(shell: &str, command: &str) -> io::Result<ExitStatus> {
    Command::new(shell).arg("-lc").arg(command).status()
}

fn run_health_check(check: &HealthCheck, interface: &str) -> SingleCheckResult {
    match check {
        HealthCheck::Ping {
            target,
            timeout_secs,
        } => run_ping_check(target, *timeout_secs, interface),
        HealthCheck::Tcp {
            target,
            port,
            timeout_secs,
        } => run_tcp_check(target, *port, *timeout_secs, interface),
        HealthCheck::Http {
            url,
            timeout_secs,
            expect_status,
            expect_contains,
        } => run_http_check(
            url,
            *timeout_secs,
            *expect_status,
            expect_contains.clone(),
            interface,
        ),
    }
}

fn run_ping_check(target: &str, timeout_secs: u64, interface: &str) -> SingleCheckResult {
    let mut command = Command::new("ping");
    command
        .arg("-c")
        .arg("1")
        .arg("-W")
        .arg(timeout_secs.to_string())
        .arg("-I")
        .arg(interface)
        .arg(target);

    match command.status() {
        Ok(status) if status.success() => SingleCheckResult {
            name: format!("ping({target}@{interface})"),
            success: true,
            detail: "连通".to_string(),
        },
        Ok(_) => SingleCheckResult {
            name: format!("ping({target}@{interface})"),
            success: false,
            detail: "失败".to_string(),
        },
        Err(err) => SingleCheckResult {
            name: format!("ping({target}@{interface})"),
            success: false,
            detail: format!("执行异常: {err}"),
        },
    }
}

fn run_tcp_check(target: &str, port: u16, timeout_secs: u64, interface: &str) -> SingleCheckResult {
    let name = format!("tcp({target}:{port}@{interface})");
    let timeout = Duration::from_secs(timeout_secs);
    match connect_tcp_via_interface(target, port, timeout, interface) {
        Ok(_) => SingleCheckResult {
            name,
            success: true,
            detail: "连通".to_string(),
        },
        Err(err) => SingleCheckResult {
            name,
            success: false,
            detail: format!("连接失败: {err}"),
        },
    }
}

fn run_http_check(
    url: &str,
    timeout_secs: u64,
    expect_status: Option<u16>,
    expect_contains: Option<String>,
    interface: &str,
) -> SingleCheckResult {
    let name = format!("http({url}@{interface})");

    let parsed = match parse_http_url(url) {
        Ok(v) => v,
        Err(err) => {
            return SingleCheckResult {
                name,
                success: false,
                detail: format!("URL 解析失败: {err}"),
            };
        }
    };

    let timeout = Duration::from_secs(timeout_secs);
    let mut stream = match connect_tcp_via_interface(&parsed.host, parsed.port, timeout, interface)
    {
        Ok(v) => v,
        Err(err) => {
            return SingleCheckResult {
                name,
                success: false,
                detail: format!("连接失败: {err}"),
            };
        }
    };

    let host_header = if parsed.port == 80 {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: warp-keeper/0.1\r\nConnection: close\r\nAccept: */*\r\n\r\n",
        parsed.path, host_header
    );

    if let Err(err) = stream.write_all(request.as_bytes()) {
        return SingleCheckResult {
            name,
            success: false,
            detail: format!("发送请求失败: {err}"),
        };
    }

    if let Err(err) = stream.set_read_timeout(Some(timeout)) {
        return SingleCheckResult {
            name,
            success: false,
            detail: format!("设置读取超时失败: {err}"),
        };
    }

    let mut raw = Vec::new();
    if let Err(err) = stream.read_to_end(&mut raw) {
        return SingleCheckResult {
            name,
            success: false,
            detail: format!("读取响应失败: {err}"),
        };
    }
    let text = String::from_utf8_lossy(&raw);

    let mut lines = text.lines();
    let status_line = lines.next().unwrap_or_default().to_string();
    let status = parse_http_status(&status_line).unwrap_or(0);
    if let Some(expected) = expect_status
        && status != expected
    {
        return SingleCheckResult {
            name,
            success: false,
            detail: format!("状态码不匹配: 实际 {status}, 期望 {expected}"),
        };
    }

    if let Some(keyword) = expect_contains {
        let body = text.split("\r\n\r\n").nth(1).unwrap_or_default();
        if !body.contains(&keyword) {
            return SingleCheckResult {
                name,
                success: false,
                detail: format!("响应体未命中关键字 `{keyword}`"),
            };
        }
    }

    SingleCheckResult {
        name,
        success: true,
        detail: format!("状态码 {status}"),
    }
}

fn connect_tcp_via_interface(
    target: &str,
    port: u16,
    timeout: Duration,
    interface: &str,
) -> io::Result<TcpStream> {
    let remote_addrs = (target, port).to_socket_addrs()?.collect::<Vec<_>>();
    if remote_addrs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "目标地址解析为空"));
    }

    let local_ips = query_interface_ipv4(interface)?;
    if local_ips.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("网卡 `{interface}` 未找到 IPv4 地址"),
        ));
    }

    let mut last_err: Option<io::Error> = None;
    for remote in remote_addrs {
        let SocketAddr::V4(remote_v4) = remote else {
            continue;
        };

        for local_ip in &local_ips {
            let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
            socket.set_read_timeout(Some(timeout))?;
            socket.set_write_timeout(Some(timeout))?;
            let local_bind = SocketAddr::new((*local_ip).into(), 0);
            socket.bind(&SockAddr::from(local_bind))?;

            match socket.connect_timeout(&SockAddr::from(SocketAddr::V4(remote_v4)), timeout) {
                Ok(_) => {
                    let stream: TcpStream = socket.into();
                    return Ok(stream);
                }
                Err(err) => {
                    last_err = Some(err);
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| io::Error::other("全部地址连接失败")))
}

fn query_interface_ipv4(interface: &str) -> io::Result<Vec<Ipv4Addr>> {
    let output = Command::new("ip")
        .arg("-4")
        .arg("-o")
        .arg("addr")
        .arg("show")
        .arg("dev")
        .arg(interface)
        .output()?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut ips = Vec::new();
    for line in text.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if let Some((idx, _)) = fields.iter().enumerate().find(|(_, v)| **v == "inet")
            && let Some(cidr) = fields.get(idx + 1)
            && let Some(addr) = cidr.split('/').next()
            && let Ok(ip) = addr.parse::<Ipv4Addr>()
        {
            ips.push(ip);
        }
    }
    Ok(ips)
}

fn parse_http_status(status_line: &str) -> Option<u16> {
    let parts = status_line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    parts[1].parse::<u16>().ok()
}

struct ParsedHttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<ParsedHttpUrl> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Err(anyhow!("仅支持 http，不支持 https"));
    }
    if !lower.starts_with("http://") {
        return Err(anyhow!("URL 必须以 http:// 开头"));
    }

    let rest = &url[7..];
    let (authority, path) = if let Some(pos) = rest.find('/') {
        (&rest[..pos], &rest[pos..])
    } else {
        (rest, "/")
    };
    if authority.is_empty() {
        return Err(anyhow!("URL 主机不能为空"));
    }

    let (host, port) = parse_host_port(authority)?;
    Ok(ParsedHttpUrl {
        host,
        port,
        path: path.to_string(),
    })
}

fn parse_host_port(authority: &str) -> Result<(String, u16)> {
    if authority.starts_with('[') {
        return Err(anyhow!("当前版本不支持 IPv6 字面量 URL"));
    }

    if let Some((host, port_str)) = authority.rsplit_once(':')
        && !host.is_empty()
    {
        let port = port_str
            .parse::<u16>()
            .with_context(|| format!("URL 端口解析失败: {port_str}"))?;
        return Ok((host.to_string(), port));
    }

    Ok((authority.to_string(), 80))
}

fn validate_check(check: &HealthCheck, path: &str) -> Result<()> {
    match check {
        HealthCheck::Ping {
            target,
            timeout_secs,
        } => {
            if target.trim().is_empty() {
                return Err(anyhow!("`{path}.target` 不能为空"));
            }
            if *timeout_secs == 0 {
                return Err(anyhow!("`{path}.timeout_secs` 必须大于 0"));
            }
        }
        HealthCheck::Tcp {
            target,
            port,
            timeout_secs,
        } => {
            if target.trim().is_empty() {
                return Err(anyhow!("`{path}.target` 不能为空"));
            }
            if *port == 0 {
                return Err(anyhow!("`{path}.port` 必须大于 0"));
            }
            if *timeout_secs == 0 {
                return Err(anyhow!("`{path}.timeout_secs` 必须大于 0"));
            }
        }
        HealthCheck::Http {
            url, timeout_secs, ..
        } => {
            if url.trim().is_empty() {
                return Err(anyhow!("`{path}.url` 不能为空"));
            }
            if !url.to_ascii_lowercase().starts_with("http://") {
                return Err(anyhow!("`{path}.url` 仅支持 http://"));
            }
            if *timeout_secs == 0 {
                return Err(anyhow!("`{path}.timeout_secs` 必须大于 0"));
            }
        }
    }
    Ok(())
}

fn list_interfaces() -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir("/sys/class/net")? {
        let item = entry?;
        if let Some(name) = item.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn interface_exists(name: &str) -> bool {
    let path = format!("/sys/class/net/{name}");
    Path::new(&path).exists()
}

fn default_reconnect_commands(client: WarpClient) -> Vec<String> {
    match client {
        WarpClient::WarpOfficial => vec![
            "warp-cli --accept-tos disconnect".to_string(),
            "warp-cli --accept-tos connect".to_string(),
        ],
        WarpClient::WarpWg => vec!["warp o".to_string(), "warp o".to_string()],
        WarpClient::WarpGo => vec!["warp-go o".to_string(), "warp-go o".to_string()],
    }
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN",
        LogLevel::Info => "INFO",
        LogLevel::Debug => "DEBUG",
    }
}

fn default_interval_secs() -> u64 {
    2
}

fn default_failure_threshold() -> u32 {
    3
}

fn default_reconnect_cooldown_secs() -> u64 {
    2
}

fn default_shell() -> String {
    "/bin/bash".to_string()
}

fn default_log_file() -> String {
    "/var/log/warp-keeper.log".to_string()
}

fn default_ping_timeout_secs() -> u64 {
    1
}

fn default_check_timeout_secs() -> u64 {
    3
}

fn default_primary_check() -> HealthCheck {
    HealthCheck::Ping {
        target: "1.1.1.1".to_string(),
        timeout_secs: default_ping_timeout_secs(),
    }
}

fn default_reconnect_verify_checks() -> Vec<HealthCheck> {
    vec![
        HealthCheck::Http {
            url: "http://www.apple.com/library/test/success.html".to_string(),
            timeout_secs: default_check_timeout_secs(),
            expect_status: Some(200),
            expect_contains: Some("success".to_string()),
        },
        HealthCheck::Tcp {
            target: "1.1.1.1".to_string(),
            port: 80,
            timeout_secs: default_check_timeout_secs(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, CommandProbe, HealthCheck, LogLevel, WarpClient, detect_client_builtin,
        parse_http_url, read_config, render_config, write_config,
    };
    use std::collections::BTreeSet;

    struct FakeProbe {
        ok_set: BTreeSet<String>,
    }

    impl CommandProbe for FakeProbe {
        fn command_ok(&self, command: &str) -> bool {
            self.ok_set.contains(command)
        }
    }

    #[test]
    fn detect_order_should_prefer_official_over_others() {
        let probe = FakeProbe {
            ok_set: [
                "systemctl is-active --quiet warp-svc".to_string(),
                "command -v warp >/dev/null 2>&1".to_string(),
                "command -v warp-go >/dev/null 2>&1".to_string(),
            ]
            .into_iter()
            .collect(),
        };
        let client = detect_client_builtin(&probe);
        assert_eq!(client, Some(WarpClient::WarpOfficial));
    }

    #[test]
    fn detect_order_should_prefer_warp_wg_over_warp_go() {
        let probe = FakeProbe {
            ok_set: [
                "command -v warp >/dev/null 2>&1".to_string(),
                "command -v warp-go >/dev/null 2>&1".to_string(),
            ]
            .into_iter()
            .collect(),
        };
        let client = detect_client_builtin(&probe);
        assert_eq!(client, Some(WarpClient::WarpWg));
    }

    #[test]
    fn parse_http_url_should_reject_https() {
        let result = parse_http_url("https://example.com");
        assert!(result.is_err());
    }

    #[test]
    fn default_config_should_support_roundtrip() {
        let config = AppConfig::default();
        let text = render_config(&config).expect("序列化应成功");
        let parsed: AppConfig = toml::from_str(&text).expect("反序列化应成功");
        assert_eq!(parsed.general.interval_secs, 2);
        assert_eq!(parsed.general.log_level, LogLevel::Info);
        match parsed.monitor.primary_check {
            HealthCheck::Ping { .. } => {}
            _ => panic!("默认主检测应为 ping"),
        }
    }

    #[test]
    fn read_write_config_should_work() {
        let tmp_dir = tempfile::tempdir().expect("创建临时目录失败");
        let path = tmp_dir.path().join("config.toml");
        let mut config = AppConfig::default();
        config.reconnect.warp_client = Some(WarpClient::WarpGo);
        write_config(&path, &config).expect("写配置失败");
        let loaded = read_config(&path).expect("读配置失败");
        assert_eq!(loaded.reconnect.warp_client, Some(WarpClient::WarpGo));
    }

    #[test]
    fn warp_client_should_be_kebab_case_in_toml() {
        let mut config = AppConfig::default();
        config.reconnect.warp_client = Some(WarpClient::WarpGo);
        let text = render_config(&config).expect("序列化应成功");
        assert!(text.contains("warp-go"));
    }
}
