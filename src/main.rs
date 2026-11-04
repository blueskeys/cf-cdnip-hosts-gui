//! cf-hosts-gui 入口:三种模式
//!   GUI(默认)/ `--auto` 无头定时模式 / `--write-hosts|--install-task|--remove-task` 提权 helper
//! 另有 `--selftest` 核心逻辑自检(不触碰系统文件)。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cfapi;
mod config;
mod hosts;
mod logger;
mod privilege;
mod selftest;
mod speedtest;

use config::{Config, LastRun};
use hosts::{HostsEntry, WriteJob, WriteMode, WriteOutcome};
use logger::Logger;
use speedtest::SpeedtestResult;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str).unwrap_or("") {
        "--write-hosts" => helper_with_outcome(args.get(2), helper_write_hosts),
        "--auto" => run_auto(),
        "--selftest" => selftest::run(),
        _ => app::run_gui(),
    }
}

// ---------- 公共小工具(app.rs 也会用) ----------

/// 域名列表 + 测速结果 → hosts 条目(固定 IP 优先,否则跟第一名;D4 一次测速全局生效)
pub fn bindings(cfg: &Config, results: &[SpeedtestResult]) -> Vec<HostsEntry> {
    let best = &results[0].ip;
    let mut map: std::collections::BTreeMap<String, String> = Default::default();
    for d in &cfg.domains {
        let name = d.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let ip = d.pin_ip.clone().unwrap_or_else(|| best.clone());
        map.insert(name, ip);
    }
    map.into_iter()
        .map(|(domain, ip)| HostsEntry { ip, domain })
        .collect()
}

/// 区块注释行摘要
pub fn meta_line(best: &SpeedtestResult) -> String {
    format!(
        "{} (best: {} {:.0}ms {:.1}MB/s {})",
        chrono::Local::now().format("%Y-%m-%d %H:%M"),
        best.ip,
        best.avg_latency_ms,
        best.speed_mb,
        best.region
    )
}

// ---------- --auto 无头模式 ----------

fn run_auto() {
    let log = Logger::new();
    log.info("== --auto 定时任务启动 ==");
    let mut cfg = Config::load();

    let results = match speedtest::run_blocking(&cfg) {
        Ok(v) => v,
        Err(e) => {
            log.error(format!("测速失败: {e}"));
            std::process::exit(2);
        }
    };
    let best = results[0].clone();
    log.info(format!(
        "测速完成: {} 条结果,最优 {} ({:.1}ms / {:.2}MB/s {})",
        results.len(),
        best.ip,
        best.avg_latency_ms,
        best.speed_mb,
        best.region
    ));

    let entries = bindings(&cfg, &results);
    if entries.is_empty() {
        log.warn("域名列表为空,跳过 hosts 写入");
        std::process::exit(0);
    }
    let job = WriteJob { mode: WriteMode::Replace, entries, meta: meta_line(&best) };

    let outcome = if privilege::is_elevated() {
        hosts::apply_job(&job)
    } else {
        // 正常应注册为最高权限计划任务;手动跑 --auto 时弹一次 UAC
        log.warn("未以管理员运行,尝试 UAC 提权写入…");
        privilege::elevate_write_hosts(&job).unwrap_or_else(|e| WriteOutcome::fail(e))
    };

    if outcome.ok {
        for d in &outcome.diff {
            log.info(d);
        }
        log.info(&outcome.message);

        // Cloudflare DNS 动态发布(启用且已配置时):把最优 IP 更新为灰云 A 记录。
        // 纯 HTTPS 调用,失败不影响 hosts 结果,只记录日志。
        if cfg.cf_dns.enabled {
            let token = cfg.cf_dns.api_token.trim().to_string();
            let domain =
                cfg.cf_dns.domain.trim().trim_end_matches('.').to_string();
            if !token.is_empty() && !domain.is_empty() {
                let zone = match &cfg.cf_dns.zone_id {
                    Some(z) => Some(z.clone()),
                    None => cfapi::resolve_zone(&token, &domain).ok(),
                };
                let dns = match zone {
                    Some(z) => {
                        let o = cfapi::upsert_record(&token, &z, &domain, &best.ip);
                        // 缓存 zone_id 供下次使用
                        if cfg.cf_dns.zone_id.as_deref() != Some(z.as_str()) {
                            cfg.cf_dns.zone_id = Some(z);
                        }
                        o
                    }
                    None => WriteOutcome::fail("Cloudflare DNS 发布失败:定位 Zone 失败"),
                };
                if dns.ok {
                    for d in &dns.diff {
                        log.info(d);
                    }
                } else {
                    log.error(&dns.message);
                }
            }
        }

        cfg.last_run = LastRun {
            best_ip: Some(best.ip.clone()),
            best_latency_ms: Some(best.avg_latency_ms),
            best_speed_mb: Some(best.speed_mb),
            best_region: Some(best.region.clone()),
            result_count: results.len(),
            at: Some(chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()),
        };
        let _ = cfg.save();
        log.info("== --auto 完成 ==");
    } else {
        log.error(format!("hosts 写入失败: {}", outcome.message));
        std::process::exit(3);
    }
}

// ---------- 提权 helper 模式 ----------

type HelperFn = fn(Option<&String>) -> WriteOutcome;

fn helper_with_outcome(job_path: Option<&String>, f: HelperFn) {
    let outcome = f(job_path);
    if let Some(p) = job_path {
        let rp = Path::new(p).with_extension("result.json");
        if let Ok(json) = serde_json::to_string(&outcome) {
            let _ = std::fs::write(&rp, json);
        }
    }
    println!("[helper] {} | {}", if outcome.ok { "OK" } else { "FAIL" }, outcome.message);
    std::process::exit(if outcome.ok { 0 } else { 1 });
}

fn helper_write_hosts(job_path: Option<&String>) -> WriteOutcome {
    let Some(p) = job_path else {
        return WriteOutcome::fail("缺少 job 文件参数");
    };
    if !privilege::is_elevated() {
        return WriteOutcome::fail("helper 未以管理员运行");
    }
    let job: WriteJob = match std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(j) => j,
        None => return WriteOutcome::fail("job 文件解析失败"),
    };
    hosts::apply_job(&job)
}


