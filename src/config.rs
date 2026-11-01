//! 配置持久化:`%APPDATA%/cf-hosts-gui/config.json`

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const APP_DIR_NAME: &str = "cf-hosts-gui";

/// 一个需要优选的域名;`pin_ip` 为手动固定的 IP(None = 自动跟第一名)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DomainEntry {
    pub name: String,
    pub pin_ip: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScheduleConfig {
    pub enabled: bool,
    pub interval_hours: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self { enabled: false, interval_hours: 24 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SpeedtestConfig {
    /// cfst.exe 绝对路径;None = 自动探测
    pub cfst_path: Option<String>,
    /// IP 段文件(相对路径按 exe/bin → exe → cwd → 各级祖先目录解析)
    pub ip_file: String,
    /// -tl 平均延迟上限 ms;0 = 不限
    pub max_latency_ms: u32,
    /// -sl 下载速度下限 MB/s;0 = 不限
    pub min_speed_mb: f64,
    /// -p 结果数量
    pub result_count: u32,
    /// -dt 每个 IP 下载测速秒数
    pub download_seconds: u32,
    /// -dd 只测延迟不做下载测速
    pub disable_download: bool,
    /// 整体超时(秒),超时强杀 cfst 进程树
    pub timeout_secs: u64,
}

impl Default for SpeedtestConfig {
    fn default() -> Self {
        Self {
            cfst_path: None,
            ip_file: "ip.txt".into(),
            max_latency_ms: 200,
            min_speed_mb: 0.0,
            result_count: 10,
            download_seconds: 10,
            disable_download: false,
            timeout_secs: 600,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LastRun {
    pub best_ip: Option<String>,
    pub best_latency_ms: Option<f64>,
    pub best_speed_mb: Option<f64>,
    pub best_region: Option<String>,
    pub result_count: usize,
    pub at: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub version: u32,
    pub domains: Vec<DomainEntry>,
    pub schedule: ScheduleConfig,
    pub speedtest: SpeedtestConfig,
    pub last_run: LastRun,
    /// UI 主题:深色(默认)/浅色
    #[serde(default = "default_dark_mode")]
    pub dark_mode: bool,
}

fn default_dark_mode() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            // 默认不含任何域名:首次启动由用户添加(私有域名不进源码,仓库可公开)
            domains: vec![],
            schedule: ScheduleConfig::default(),
            speedtest: SpeedtestConfig::default(),
            last_run: LastRun::default(),
            dark_mode: true,
        }
    }
}

/// 数据目录:Windows 用 %APPDATA%,其余平台回退到 ~/.local/share
pub fn app_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/share")
        });
    base.join(APP_DIR_NAME)
}

impl Config {
    pub fn path() -> PathBuf {
        app_dir().join("config.json")
    }

    pub fn load() -> Config {
        match fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("配置解析失败,使用默认配置: {e}");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = app_dir();
        fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        // 原子写:先写临时文件再替换,避免写一半损坏
        let tmp = dir.join("config.json.tmp");
        fs::write(&tmp, json.as_bytes())?;
        fs::rename(&tmp, Self::path())?;
        Ok(())
    }
}
