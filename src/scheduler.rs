//! 定时机制(D3):封装 schtasks 注册/移除/查询计划任务,最高权限运行 `--auto`。
//! 注册动作需要管理员,由 privilege 模块的提权 helper 调用本模块。

use crate::hosts::run_hidden;

pub const TASK_NAME: &str = "cf-hosts-gui";

#[cfg(windows)]
pub fn install(interval_hours: u32) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let tr = format!("\"{}\" --auto", exe.to_string_lossy());
    let out = run_hidden(
        "schtasks",
        &[
            "/Create",
            "/F",
            "/TN",
            TASK_NAME,
            "/TR",
            &tr,
            "/SC",
            "HOURLY",
            "/MO",
            &interval_hours.max(1).to_string(),
            "/RL",
            "HIGHEST",
        ],
    )
    .map_err(|e| format!("schtasks 启动失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "注册计划任务失败: {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub fn remove() -> Result<(), String> {
    let out = run_hidden("schtasks", &["/Delete", "/F", "/TN", TASK_NAME])
        .map_err(|e| format!("schtasks 启动失败: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        // 任务本就不存在时视为成功
        if stderr.contains("找不到") || stderr.to_lowercase().contains("cannot find") {
            return Ok(());
        }
        return Err(format!("移除计划任务失败: {stderr}"));
    }
    Ok(())
}

/// 查询任务是否已注册(普通权限即可)
#[cfg(windows)]
pub fn is_installed() -> bool {
    run_hidden("schtasks", &["/Query", "/TN", TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn install(_interval_hours: u32) -> Result<(), String> {
    Err("计划任务功能当前仅支持 Windows(macOS 将用 launchd 实现)".into())
}
#[cfg(not(windows))]
pub fn remove() -> Result<(), String> {
    Err("计划任务功能当前仅支持 Windows(macOS 将用 launchd 实现)".into())
}
#[cfg(not(windows))]
pub fn is_installed() -> bool {
    false
}
