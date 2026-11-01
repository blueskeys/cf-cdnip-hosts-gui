//! cf-hosts-gui 入口:三种模式
//!   GUI(默认)/ `--auto` 无头定时模式 / `--write-hosts|--install-task|--remove-task` 提权 helper
//! 另有 `--selftest` 核心逻辑自检(不触碰系统文件)。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod hosts;
mod logger;
mod privilege;
mod scheduler;
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
        "--install-task" => helper_with_outcome(args.get(2), helper_install_task),
        "--remove-task" => helper_with_outcome(args.get(2), helper_remove_task),
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

fn helper_install_task(job_path: Option<&String>) -> WriteOutcome {
    if !privilege::is_elevated() {
        return WriteOutcome::fail("helper 未以管理员运行");
    }
    let payload: serde_json::Value = job_path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({ "interval_hours": 24 }));
    let hours = payload
        .get("interval_hours")
        .and_then(|v| v.as_u64())
        .unwrap_or(24) as u32;
    match scheduler::install(hours) {
        Ok(()) => WriteOutcome {
            ok: true,
            message: format!("计划任务已注册(每 {hours} 小时,最高权限)"),
            diff: vec![],
        },
        Err(e) => WriteOutcome::fail(e),
    }
}

fn helper_remove_task(_job_path: Option<&String>) -> WriteOutcome {
    if !privilege::is_elevated() {
        return WriteOutcome::fail("helper 未以管理员运行");
    }
    match scheduler::remove() {
        Ok(()) => WriteOutcome { ok: true, message: "计划任务已移除".into(), diff: vec![] },
        Err(e) => WriteOutcome::fail(e),
    }
}
