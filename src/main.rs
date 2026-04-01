use clap::{Parser, Subcommand};
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;
use warp_keeper::{
    AppConfig, CheckReport, Logger, detect_client_now, execute_reconnect, find_warp_interface,
    init_config, load_or_create_config, run_primary_check, run_reconnect_verify_checks,
};

#[derive(Debug, Parser)]
#[command(name = "warp-keeper", version, about = "WARP 断线检测与自动重连工具")]
struct Cli {
    #[arg(short, long, default_value = "./config.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand, Clone, Copy)]
enum Commands {
    /// 持续检测并在断线时重连
    Run,
    /// 仅执行一次 ICMP 检测
    Check,
    /// 初始化配置文件
    Init {
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// 手动执行一次客户端识别并写入重连命令
    Detect,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("[ERROR] {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<u8> {
    let command = cli.command.unwrap_or(Commands::Run);
    match command {
        Commands::Init { force } => {
            let _ = init_config(&cli.config, force)?;
            println!("配置文件已初始化: {}", cli.config.display());
            Ok(0)
        }
        Commands::Detect => {
            let mut config = load_or_create_config(&cli.config)?;
            let logger = make_logger(&config);
            let detected = detect_client_now(&cli.config, &mut config)?;
            match detected {
                Some(client) => logger.info(&format!(
                    "识别到客户端: {}，已写入 reconnect.commands",
                    client
                )),
                None => logger.warn("未识别到客户端，已清空 reconnect.commands，请手动填写"),
            }
            Ok(0)
        }
        Commands::Check => run_check(&cli.config),
        Commands::Run => run_loop(&cli.config),
    }
}

fn run_check(config_path: &Path) -> anyhow::Result<u8> {
    let config = load_or_create_config(config_path)?;
    let logger = make_logger(&config);

    let interface = resolve_interface_or_fail(&config, &logger)?;
    let result = run_primary_check(&config, &interface);
    log_single_check(&logger, &result);

    if result.success { Ok(0) } else { Ok(1) }
}

fn run_loop(config_path: &Path) -> anyhow::Result<u8> {
    let config = load_or_create_config(config_path)?;
    let logger = make_logger(&config);

    let mut interface = resolve_interface_or_fail(&config, &logger)?;
    logger.info(&format!("使用 WARP 网卡: {interface}"));

    let mut consecutive_failures: u32 = 0;
    logger.info("开始监控循环");

    loop {
        let primary = run_primary_check(&config, &interface);
        log_single_check(&logger, &primary);

        if primary.success {
            consecutive_failures = 0;
            thread::sleep(Duration::from_secs(config.general.interval_secs));
            continue;
        }

        consecutive_failures = consecutive_failures.saturating_add(1);
        logger.warn(&format!(
            "连续失败次数: {}/{}",
            consecutive_failures, config.general.failure_threshold
        ));

        if consecutive_failures < config.general.failure_threshold {
            thread::sleep(Duration::from_secs(config.general.interval_secs));
            continue;
        }

        if config.reconnect.commands.is_empty() {
            logger.error("reconnect.commands 为空，无法重连。请先执行 detect 或手动填写命令。");
            consecutive_failures = 0;
            thread::sleep(Duration::from_secs(config.general.interval_secs));
            continue;
        }

        logger.warn("开始执行重连命令序列");
        match execute_reconnect(&config) {
            Ok(_) => {
                logger.info("重连命令执行完成");
                if config.general.reconnect_cooldown_secs > 0 {
                    thread::sleep(Duration::from_secs(config.general.reconnect_cooldown_secs));
                }

                if let Ok(Some(new_iface)) = find_warp_interface(&config)
                    && new_iface != interface
                {
                    interface = new_iface;
                    logger.info(&format!("重连后更新网卡为: {interface}"));
                }

                let verify = run_reconnect_verify_checks(&config, &interface);
                log_report(&logger, &verify);
                if verify.all_success {
                    logger.info("重连后检测全部成功，恢复主检测循环");
                } else {
                    logger.warn("重连后检测有失败，将继续主循环检测");
                }
            }
            Err(err) => logger.error(&format!("重连执行失败: {err:#}")),
        }

        consecutive_failures = 0;
        thread::sleep(Duration::from_secs(config.general.interval_secs));
    }
}

fn resolve_interface_or_fail(config: &AppConfig, logger: &Logger) -> anyhow::Result<String> {
    match find_warp_interface(config)? {
        Some(name) => Ok(name),
        None => {
            logger.error("未找到名称包含 `warp` 的网卡（不区分大小写）");
            Err(anyhow::anyhow!(
                "请确认 WARP 已连接，或在 `monitor.interface_name` 手动指定网卡名"
            ))
        }
    }
}

fn log_single_check(logger: &Logger, result: &warp_keeper::SingleCheckResult) {
    if result.success {
        logger.info(&format!("[OK] {} -> {}", result.name, result.detail));
    } else {
        logger.warn(&format!("[FAIL] {} -> {}", result.name, result.detail));
    }
}

fn log_report(logger: &Logger, report: &CheckReport) {
    for item in &report.checks {
        log_single_check(logger, item);
    }
}

fn make_logger(config: &AppConfig) -> Logger {
    match Logger::from_config(&config.general) {
        Ok(logger) => logger,
        Err(err) => {
            eprintln!(
                "[WARN] 初始化日志文件 `{}` 失败，后续仅输出到终端，不写入磁盘: {err:#}",
                config.general.log_file
            );
            Logger::console_only(config.general.log_level)
        }
    }
}
