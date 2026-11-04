//! cfst.exe 子进程封装:参数映射、阶段进度(stdout 文案匹配)、CSV 解析、超时强杀。
//! 设计依据 cfst v2.2.5 源码:阶段文案 "开始延迟测速"/"开始下载测速"/"完整测速结果已写入",
//! CSV 表头 `IP 地址,已发送,已接收,丢包率,平均延迟,下载速度(MB/s),地区码`(UTF-8 + BOM)。

use crate::config::{app_dir, Config};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const STAGE_LATENCY: &str = "开始延迟测速";
const STAGE_DOWNLOAD: &str = "开始下载测速";
const STAGE_WRITTEN: &str = "完整测速结果已写入";

#[cfg(windows)]
const CFST_NAME: &str = "cfst.exe";
#[cfg(not(windows))]
const CFST_NAME: &str = "cfst";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeedtestResult {
    pub ip: String,
    pub loss_rate: f64,
    pub avg_latency_ms: f64,
    pub speed_mb: f64,
    pub region: String,
}

/// 推给 UI 的事件
pub enum SpeedtestEvent {
    Stage(String),
    /// 阶段进度(phase 0=延迟 1=下载):cfst 进度条在管道模式下无换行,
    /// 按 "N / M [...] 备注" 渲染序列解析,取最新一次
    Progress { phase: u8, done: u32, total: u32, note: String },
    Line(String),
    Done(Vec<SpeedtestResult>),
    Failed(String),
}

/// 测速结果输出文件(放在数据目录,避免污染 exe 目录)
/// 支持环境变量 CFHOSTS_RESULT_CSV 覆盖 —— 测试用它隔离,避免动用户真实数据
pub fn result_csv_path() -> PathBuf {
    if let Ok(p) = std::env::var("CFHOSTS_RESULT_CSV") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    app_dir().join("result.csv")
}

/// 探测 cfst.exe:显式配置 → exe/bin → exe → cwd/bin → cwd → 各级祖先下的 cfst_windows_amd64/
pub fn resolve_cfst(cfg: &Config) -> Option<PathBuf> {
    if let Some(p) = &cfg.speedtest.cfst_path {
        let path = Path::new(p);
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("bin").join(CFST_NAME));
            candidates.push(dir.join(CFST_NAME));
            for anc in dir.ancestors().skip(1).take(4) {
                candidates.push(anc.join("cfst_windows_amd64").join(CFST_NAME));
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    candidates.push(cwd.join("bin").join(CFST_NAME));
    candidates.push(cwd.join(CFST_NAME));
    candidates.into_iter().find(|p| p.is_file())
}

/// 解析 IP 段文件路径(相对路径按与 cfst 相同的候选顺序探测)
pub fn resolve_ip_file(cfg: &Config) -> Option<PathBuf> {
    let raw = Path::new(&cfg.speedtest.ip_file);
    if raw.is_file() {
        return Some(raw.to_path_buf());
    }
    let name = raw.file_name().unwrap_or_default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("bin").join(name));
            candidates.push(dir.join(name));
            for anc in dir.ancestors().skip(1).take(4) {
                candidates.push(anc.join("cfst_windows_amd64").join(name));
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    candidates.push(cwd.join("bin").join(name));
    candidates.push(cwd.join(name));
    candidates.into_iter().find(|p| p.is_file())
}

/// GUI 设置 → cfst 命令行参数映射(D1.1)
pub fn build_args(cfg: &Config, ip_file: &Path, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-f".into(),
        ip_file.to_string_lossy().into_owned(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
        // -p 仅控制控制台打印行数,CSV 恒为全量输出;固定 0 = 不打印,
        // 且可跳过 cfst 在 Windows 下的"按下回车键退出"等待(endPrint → Scanln 阻塞)
        "-p".into(),
        "0".into(),
    ];
    if cfg.speedtest.max_latency_ms > 0 {
        a.push("-tl".into());
        a.push(cfg.speedtest.max_latency_ms.to_string());
    }
    if cfg.speedtest.min_speed_mb > 0.0 {
        a.push("-sl".into());
        a.push(format!("{:.2}", cfg.speedtest.min_speed_mb));
    }
    if cfg.speedtest.disable_download {
        a.push("-dd".into());
    } else {
        a.push("-dt".into());
        a.push(cfg.speedtest.download_seconds.max(1).to_string());
    }
    a
}

/// 后台线程执行测速(GUI 模式)
pub fn spawn_speedtest<F>(cfg: &Config, mut on_event: F)
where
    F: FnMut(SpeedtestEvent) + Send + 'static,
{
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        if let Err(e) = run_once(&cfg, &mut on_event) {
            on_event(SpeedtestEvent::Failed(e));
        }
    });
}

/// 阻塞执行(--auto 无头模式):返回结果列表
pub fn run_blocking(cfg: &Config) -> Result<Vec<SpeedtestResult>, String> {
    let (tx, rx) = std::sync::mpsc::channel::<SpeedtestEvent>();
    let cfg2 = cfg.clone();
    std::thread::spawn(move || {
        if let Err(e) = run_once(&cfg2, &mut |ev| {
            let _ = tx.send(ev);
        }) {
            let _ = tx.send(SpeedtestEvent::Failed(e));
        }
    });
    loop {
        match rx.recv() {
            Ok(SpeedtestEvent::Done(v)) => return Ok(v),
            Ok(SpeedtestEvent::Failed(e)) => return Err(e),
            Ok(_) => {}
            Err(_) => return Err("测速线程异常退出".into()),
        }
    }
}

fn run_once(cfg: &Config, emit: &mut impl FnMut(SpeedtestEvent)) -> Result<(), String> {
    let cfst =
        resolve_cfst(cfg).ok_or_else(|| "未找到 cfst.exe,请在设置页指定完整路径".to_string())?;
    let ip_file = resolve_ip_file(cfg).ok_or_else(|| "未找到 IP 段文件 ip.txt".to_string())?;
    let out = result_csv_path();
    let args = build_args(cfg, &ip_file, &out);

    let _ = std::fs::remove_file(&out);
    emit(SpeedtestEvent::Stage(format!(
        "启动 {}",
        cfst.file_name().unwrap_or_default().to_string_lossy()
    )));
    let mut cmd = Command::new(&cfst);
    cmd.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // cfst 是控制台程序:GUI(窗口子系统)拉起时必须 CREATE_NO_WINDOW,否则会弹控制台窗口
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动 cfst 失败: {e}"))?;
    // 立即关闭 stdin(EOF):cfst 在 Windows 下结束时等回车(Scanln),无控制台时给 EOF 让它直接退出
    if let Some(stdin) = child.stdin.take() {
        drop(stdin);
    }

    let pid = child.id();
    let finished = Arc::new(AtomicBool::new(false));
    // 看门狗:整体超时强杀进程树(cfst 卡死时兜底)
    {
        let finished = finished.clone();
        let timeout = Duration::from_secs(cfg.speedtest.timeout_secs.max(60));
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            if !finished.load(Ordering::SeqCst) {
                kill_tree(pid);
            }
        });
    }

    // 双流读取(实测 cfst v2.3.5):
    //   stdout = 表头/阶段行/结果消息(按行,\n 结尾)
    //   stderr = cheggaaa/pb 进度条 —— 非终端模式无 \r 无 \n,每次渲染直接追加
    // 两个读线程灌入同一 channel,单消费者统一解析(emit 只在消费者侧,满足 FnMut)
    let (tx_raw, rx_raw) = std::sync::mpsc::channel::<Raw>();
    if let Some(s) = child.stdout.take() {
        pump_stream(s, false, tx_raw.clone());
    }
    if let Some(s) = child.stderr.take() {
        pump_stream(s, true, tx_raw.clone());
    }
    drop(tx_raw);
    let mut out_buf: Vec<u8> = Vec::new();
    let mut err_buf = String::new();
    let mut phase = 0u8;
    while let Ok(raw) = rx_raw.recv() {
        match raw {
            Raw::Out(b) => {
                out_buf.extend_from_slice(&b);
                while let Some(pos) = out_buf.iter().position(|&c| c == b'\n') {
                    let line: Vec<u8> = out_buf.drain(..=pos).collect();
                    let clean = strip_ansi(&String::from_utf8_lossy(&line));
                    if clean.contains(STAGE_LATENCY) {
                        phase = 0;
                        emit(SpeedtestEvent::Stage("延迟测速中…".into()));
                    } else if clean.contains(STAGE_DOWNLOAD) {
                        phase = 1;
                        emit(SpeedtestEvent::Stage("下载测速中…".into()));
                    } else if clean.contains(STAGE_WRITTEN) {
                        emit(SpeedtestEvent::Stage("测速完成,读取结果".into()));
                    }
                    if let Some((done, total, note)) = parse_progress(&clean) {
                        // 兼容未来 cfst 把进度条挪回 stdout
                        emit(SpeedtestEvent::Progress { phase, done, total, note });
                    } else {
                        emit(SpeedtestEvent::Line(clean));
                    }
                }
            }
            Raw::Err(b) => {
                err_buf.push_str(&String::from_utf8_lossy(&b));
                if let Some((done, total, note)) = parse_progress(err_buf.trim()) {
                    emit(SpeedtestEvent::Progress { phase, done, total, note });
                }
                // 防御性截断:阶段渲染约 2~4KB,异常增长时只留尾部
                if err_buf.len() > 64 * 1024 {
                    let keep = err_buf.split_off(err_buf.len() - 2048);
                    err_buf = keep;
                }
            }
        }
    }

    let status = child.wait().map_err(|e| format!("等待 cfst 退出失败: {e}"))?;
    finished.store(true, Ordering::SeqCst);
    if !status.success() {
        return Err(format!("cfst 异常退出(exit={})", status.code().unwrap_or(-1)));
    }
    let mut results = parse_csv(&out)?;
    if results.is_empty() {
        // 异常处理要点:0 条结果时保留现有 hosts 不动
        return Err("cfst 返回 0 条结果(网络波动?),已保留现有 hosts".into());
    }
    // CSV 是全量输出,按用户设置的 -p 数量截断展示
    results.truncate(cfg.speedtest.result_count.max(1) as usize);
    emit(SpeedtestEvent::Done(results));
    Ok(())
}

/// 子进程原始字节流标签(stdout=消息行 / stderr=进度条)
enum Raw {
    Out(Vec<u8>),
    Err(Vec<u8>),
}

/// 读线程:把流按块灌入 channel,EOF/断开即退出
fn pump_stream<R: Read + Send + 'static>(mut stream: R, err: bool, tx: std::sync::mpsc::Sender<Raw>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let bytes = chunk[..n].to_vec();
                    let msg = if err { Raw::Err(bytes) } else { Raw::Out(bytes) };
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// 解析 cfst 进度条渲染序列 "N / M [----____] 备注",取最后一次
/// (延迟阶段备注为 "可用: K",下载阶段为空;正常消息行不含 " N / M [" 不会误判)
fn parse_progress(s: &str) -> Option<(u32, u32, String)> {
    let mut best: Option<(u32, u32, String)> = None;
    let mut from = 0;
    while let Some(rel) = s[from..].find(" / ") {
        let sep = from + rel;
        from = sep + 3;
        let done: u32 = match s[..sep]
            .trim_end()
            .rsplit(char::is_whitespace)
            .next()
            .and_then(|t| t.parse().ok())
        {
            Some(v) => v,
            None => continue,
        };
        let right = &s[sep + 3..];
        let total: u32 = match right
            .trim_start()
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("")
            .parse()
            .ok()
        {
            Some(v) => v,
            None => continue,
        };
        if total == 0 || !right.contains(']') {
            continue;
        }
        // 备注取 ']' 之后到双空格(下一次渲染开始)为止
        let note = right
            .find(']')
            .map(|e| right[e + 1..].trim())
            .unwrap_or("")
            .split("  ")
            .next()
            .unwrap_or("")
            .to_string();
        best = Some((done, total, note));
    }
    best
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .creation_flags(0x0800_0000)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(not(windows))]
fn kill_tree(pid: u32) {
    let _ = Command::new("pkill")
        .args(["-TERM", "-P", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// 解析 cfst 的 result.csv(UTF-8 + BOM);兼容 -dd 模式的 5 列无速度格式
pub fn parse_csv(path: &Path) -> Result<Vec<SpeedtestResult>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取结果文件失败: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|c| c.trim().trim_matches('"')).collect();
        if i == 0 || cols[0].contains("IP") {
            continue; // 表头
        }
        if cols.len() < 5 {
            continue;
        }
        let ip = cols[0].to_string();
        let looks_ip = ip.split('.').count() == 4 || ip.contains(':');
        if !looks_ip {
            continue;
        }
        let loss = cols[3].parse().unwrap_or(0.0);
        let lat = cols[4].parse().unwrap_or(0.0);
        let spd = cols.get(5).and_then(|c| c.parse().ok()).unwrap_or(0.0);
        let region = cols.get(6).unwrap_or(&"").to_string();
        out.push(SpeedtestResult {
            ip,
            loss_rate: loss,
            avg_latency_ms: lat,
            speed_mb: spd,
            region,
        });
    }
    Ok(out)
}

/// 去除 ANSI 转义序列(cfst 输出带颜色码)
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some('[') = chars.next() {
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_progress_takes_latest_render() {
        // 真实抓包样本片段:多次渲染连成一条,无换行
        let raw = "0 / 320 [_____] 可用:   0   3 / 320 [ __] 可用: 3  22 / 320 [--↙__] 可用: 22  ";
        assert_eq!(parse_progress(raw), Some((22, 320, "可用: 22".to_string())));
        // 单次渲染
        assert_eq!(parse_progress("22 / 40 [----] 可用: 4 "), Some((22, 40, "可用: 4".to_string())));
    }

    #[test]
    fn parse_progress_ignores_normal_lines() {
        assert_eq!(parse_progress("# XIU2/CloudflareSpeedTest v2.3.5 "), None);
        assert_eq!(
            parse_progress("开始延迟测速（模式：TCP, 端口：443, 范围：0 ~ 9999 ms, 丢包：1.00)"),
            None
        );
        assert_eq!(parse_progress("完整测速结果（延迟测速 + 下载测速）已写入 CSV 文件: C:/x/result.csv"), None);
    }

    /// 真实端到端:10 个 IP 跑一次 cfst(-dd 只测延迟,约 15 秒),
    /// 断言双流读取能实时解析出 Progress 事件(而非等阶段结束)
    /// 输出走 CFHOSTS_RESULT_CSV 沙箱,绝不动用户真实 result.csv
    #[test]
    fn real_speedtest_emits_progress_events() {
        // 本机开发环境才跑真 cfst:复用 resolve_cfst 的相对路径探测
        // (exe/bin → exe → cwd/bin → cwd → 各级祖先下的 cfst_windows_amd64/);
        // 探测不到(CI/其他机器)时跳过,只跑纯解析测试
        let cfst = resolve_cfst(&Config::default())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if cfst.is_empty() {
            eprintln!("跳过:未探测到本机 cfst.exe(resolve_cfst 相对路径候选均未命中)");
            return;
        }
        let sandbox_csv = std::env::temp_dir().join("cfhost-gui-test-result.csv");
        let _ = std::fs::remove_file(&sandbox_csv);
        std::env::set_var("CFHOSTS_RESULT_CSV", &sandbox_csv);
        let tiny = std::env::temp_dir().join("cfhost-gui-test-ip.txt");
        std::fs::write(
            &tiny,
            "104.16.0.0\n104.17.0.0\n172.64.0.0\n162.159.0.0\n108.162.0.0\n103.21.244.0\n141.101.0.0\n162.158.0.0\n188.114.96.0\n190.93.240.0\n",
        )
        .unwrap();
        let cfg = crate::config::Config {
            speedtest: crate::config::SpeedtestConfig {
                cfst_path: Some(cfst.into()),
                ip_file: tiny.to_string_lossy().into_owned(),
                max_latency_ms: 0,
                min_speed_mb: 0.0,
                result_count: 5,
                download_seconds: 5,
                disable_download: true,
                timeout_secs: 90,
            },
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let res = run_once(&cfg, &mut |ev| {
            let _ = tx.send(ev);
        });
        res.expect("cfst 运行失败");

        let mut got = 0;
        let mut max_done = 0;
        while let Ok(ev) = rx.try_recv() {
            if let SpeedtestEvent::Progress { phase, done, total, note } = ev {
                got += 1;
                assert_eq!(phase, 0, "-dd 模式应只有延迟阶段");
                assert!(total > 0 && done <= total);
                max_done = max_done.max(done);
                assert!(note.contains("可用") || note.is_empty(), "备注应为可用计数或空: {note}");
            }
        }
        assert!(got >= 2, "应实时收到多个 Progress 事件(而非仅阶段结束一次),实际 {got}");
        assert!(max_done > 0);
    }
}
