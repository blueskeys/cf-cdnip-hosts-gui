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

/// 内置定时器(程序运行期间生效,跨平台,不依赖系统计划任务)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScheduleConfig {
    pub enabled: bool,
    /// 触发间隔(分钟)
    #[serde(default = "default_interval_minutes")]
    pub interval_minutes: u32,
}

fn default_interval_minutes() -> u32 {
    120
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self { enabled: false, interval_minutes: 120 }
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

/// Cloudflare DNS 动态发布:把当前最优 IP 更新到一条灰云 A 记录,
/// 供外部工具/内网 DNS 通过普通 DNS 查询获取优选 IP。
/// 默认全部留空:域名与 Token 属于用户私有信息,不进源码与默认配置。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CfDnsConfig {
    pub enabled: bool,
    /// Cloudflare API Token(建议仅授 Zone.DNS Edit 权限)
    pub api_token: String,
    /// 发布域名(如 vip.example.com);A 记录强制灰云(proxied=false)
    pub domain: String,
    /// zone_id 缓存(「验证连接」时自动填写,避免每次逐级查询)
    pub zone_id: Option<String>,
}

impl Default for CfDnsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_token: String::new(),
            domain: String::new(),
            zone_id: None,
        }
    }
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
    /// Cloudflare DNS 动态发布(默认关闭)
    #[serde(default)]
    pub cf_dns: CfDnsConfig,
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
            cf_dns: CfDnsConfig::default(),
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
