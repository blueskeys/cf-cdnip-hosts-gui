//! 权限模型(D2):GUI 普通权限运行;写 hosts 时以 runas(UAC)重启自身进入
//! `--write-hosts <job.json>` helper 模式,helper 完成后把结果写到 `<job>.result.json` 并退出。

use crate::hosts::{WriteJob, WriteOutcome};
use std::path::{Path, PathBuf};

/// 当前进程是否有管理员权限
#[cfg(windows)]
pub fn is_elevated() -> bool {
    // 管理员组成员执行 `net session` 返回 0
    crate::hosts::run_hidden("net", &["session"])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn jobs_dir() -> PathBuf {
    let d = crate::config::app_dir().join("jobs");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn job_path(tag: &str) -> PathBuf {
    jobs_dir().join(format!(
        "{tag}-{}.json",
        chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")
    ))
}

fn outcome_path(job: &Path) -> PathBuf {
    job.with_extension("result.json")
}

/// 提权执行 helper(阻塞直至 helper 进程退出),返回其执行结果
fn elevate(flag: &str, payload_json: String) -> Result<WriteOutcome, String> {
    let job = job_path(flag.trim_start_matches('-'));
    std::fs::write(&job, payload_json).map_err(|e| format!("写入任务文件失败: {e}"))?;

    let exe = std::env::current_exe().map_err(|e| format!("获取自身路径失败: {e}"))?;
    let status = runas::Command::new(&exe)
        .arg(flag)
        .arg(&job)
        .status()
        .map_err(|e| {
            if e.raw_os_error() == Some(1223) {
                "已取消 UAC 授权".to_string()
            } else {
                format!("提权启动失败: {e}")
            }
        })?;

    let outcome = read_outcome(&outcome_path(&job));
    if !status.success() && outcome.is_err() {
        // helper 非零退出且没留结果文件:至少把退出码报出来
        return Ok(WriteOutcome::fail(format!(
            "helper 异常退出(exit={})",
            status.code().unwrap_or(-1)
        )));
    }
    outcome
}

fn read_outcome(p: &Path) -> Result<WriteOutcome, String> {
    let s = std::fs::read_to_string(p).map_err(|_| "未读到 helper 结果文件".to_string())?;
    serde_json::from_str(&s).map_err(|e| format!("解析 helper 结果失败: {e}"))
}

/// 提权写 hosts(弹一次 UAC)
pub fn elevate_write_hosts(job: &WriteJob) -> Result<WriteOutcome, String> {
    let json = serde_json::to_string(job).map_err(|e| e.to_string())?;
    elevate("--write-hosts", json)
}

/// 提权注册计划任务
pub fn elevate_install_task(interval_hours: u32) -> Result<WriteOutcome, String> {
    let json = serde_json::json!({ "interval_hours": interval_hours }).to_string();
    elevate("--install-task", json)
}

/// 提权移除计划任务
pub fn elevate_remove_task() -> Result<WriteOutcome, String> {
    elevate("--remove-task", "{}".into())
}
