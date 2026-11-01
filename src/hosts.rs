//! hosts 区块化读写:程序写入的记录全部包裹在 BEGIN/END 标记内,只动自己的区块。
//! 写入流程:备份 → 写入 → 读回校验 → 刷新 DNS;任一步失败自动回滚备份。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

pub const MARK_BEGIN: &str = "# --- cf-hosts-gui BEGIN ---";
pub const MARK_END: &str = "# --- cf-hosts-gui END ---";

// ---------- 平台抽象(M1 仅 Windows 实现,接口为 macOS 预留) ----------

#[cfg(windows)]
pub fn hosts_path() -> PathBuf {
    PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts")
}
#[cfg(not(windows))]
pub fn hosts_path() -> PathBuf {
    PathBuf::from("/etc/hosts")
}

#[cfg(windows)]
pub fn flush_dns() -> std::io::Result<()> {
    run_hidden("ipconfig", &["/flushdns"]).map(|_| ())
}
#[cfg(not(windows))]
pub fn flush_dns() -> std::io::Result<()> {
    let _ = Command::new("dscacheutil").arg("-flushcache").status();
    let _ = Command::new("killall").arg("-HUP").arg("mDNSResponder").status();
    Ok(())
}

/// 执行系统命令但不闪控制台窗口(Windows CREATE_NO_WINDOW)
#[cfg(windows)]
pub fn run_hidden(prog: &str, args: &[&str]) -> std::io::Result<Output> {
    use std::os::windows::process::CommandExt;
    Command::new(prog)
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
}
#[cfg(not(windows))]
pub fn run_hidden(prog: &str, args: &[&str]) -> std::io::Result<Output> {
    Command::new(prog).args(args).output()
}

// ---------- 数据结构 ----------

/// 一条 hosts 记录:IP + 域名
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HostsEntry {
    pub ip: String,
    pub domain: String,
}

/// 提权写入任务(GUI / --auto → 提权 helper 的 job.json)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteJob {
    pub mode: WriteMode,
    /// Replace 模式:写入管理区块的全部条目
    pub entries: Vec<HostsEntry>,
    /// 区块内注释行(时间 + 最优 IP 摘要)
    pub meta: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WriteMode {
    /// 写入/更新管理区块
    Replace,
    /// 移除管理区块(还原)
    RemoveAll,
}

/// helper 的执行结果(job.result.json,GUI 轮询读取)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteOutcome {
    pub ok: bool,
    pub message: String,
    /// 变更摘要(逐条 diff,写日志)
    pub diff: Vec<String>,
}

impl WriteOutcome {
    pub fn fail(msg: impl Into<String>) -> WriteOutcome {
        WriteOutcome { ok: false, message: msg.into(), diff: vec![] }
    }
}

// ---------- 计划构建(纯函数,便于测试) ----------

/// 对当前 hosts 内容生成新内容(不落盘)
pub fn build_plan(current: &str, job: &WriteJob) -> HostsPlan {
    let crlf = current.matches("\r\n").count() > current.matches('\n').count() / 2;
    let nl = if crlf { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = current
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    // 收敛末尾空行(保留一行)
    while lines.len() > 1 && lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }

    let mut diff: Vec<String> = Vec::new();
    let domains: Vec<&str> = job.entries.iter().map(|e| e.domain.as_str()).collect();

    // 1) 注释掉区块外与管理域名冲突的行(如此前手工加的 hosts 记录)
    let (begin, end) = locate_block(&lines);
    for i in 0..lines.len() {
        if inside(i, begin, end) {
            continue;
        }
        let t = lines[i].trim().to_string();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut it = t.split_whitespace();
        let (Some(_ip), Some(dom), _) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if domains.iter().any(|d| d.eq_ignore_ascii_case(dom)) {
            diff.push(format!("注释区块外冲突行: {t}"));
            lines[i] = format!("# {t}  # [cf-hosts-gui] 已由管理区块接管");
        }
    }

    // 2) 生成新区块内容
    let block_lines: Vec<String> = match job.mode {
        WriteMode::Replace => {
            let mut b = vec![MARK_BEGIN.to_string(), format!("# updated: {}", job.meta)];
            // 同域名去重(后者覆盖前者)
            let mut seen: Vec<&str> = Vec::new();
            for ent in job.entries.iter().rev() {
                if seen.iter().any(|d| d.eq_ignore_ascii_case(&ent.domain)) {
                    continue;
                }
                seen.push(&ent.domain);
                b.push(format!("{} {}", ent.ip, ent.domain));
            }
            b.push(MARK_END.to_string());
            diff.push(format!("写入管理区块: {} 条记录", seen.len()));
            b
        }
        WriteMode::RemoveAll => {
            if begin.is_some() {
                diff.push("移除管理区块".into());
            }
            vec![]
        }
    };

    // 3) 拼装:原地替换旧区块,或追加到文件末尾
    let mut new_lines: Vec<String>;
    match locate_block(&lines) {
        (Some(b), Some(e)) => {
            new_lines = lines[..b].to_vec();
            if job.mode == WriteMode::Replace {
                new_lines.extend(block_lines);
            }
            new_lines.extend(lines[e + 1..].to_vec());
        }
        _ => {
            new_lines = lines.clone();
            if job.mode == WriteMode::Replace {
                new_lines.push(String::new()); // 与原有内容空一行
                new_lines.extend(block_lines);
            }
        }
    }
    while new_lines.len() > 1
        && new_lines.last().map(|l| l.trim().is_empty()).unwrap_or(false)
    {
        new_lines.pop();
    }
    new_lines.push(String::new()); // 文件以换行结尾

    let new_content = new_lines.join(nl);
    let changed = new_content != normalize(current, nl);
    HostsPlan { new_content, changed, diff }
}

/// 与 build_plan 相同的规范化(判断"内容是否真的变化")
fn normalize(s: &str, nl: &str) -> String {
    let mut lines: Vec<String> = s
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    while lines.len() > 1 && lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.push(String::new());
    lines.join(nl)
}

pub struct HostsPlan {
    pub new_content: String,
    pub changed: bool,
    pub diff: Vec<String>,
}

/// 定位管理区块(起止行号,含标记行;不完整则视为不存在)
fn locate_block(lines: &[String]) -> (Option<usize>, Option<usize>) {
    let b = lines.iter().position(|l| l.trim() == MARK_BEGIN);
    let e = lines.iter().position(|l| l.trim() == MARK_END);
    if let (Some(b), Some(e)) = (b, e) {
        if b < e {
            return (Some(b), Some(e));
        }
    }
    (None, None)
}

fn inside(i: usize, b: Option<usize>, e: Option<usize>) -> bool {
    matches!((b, e), (Some(b), Some(e)) if i >= b && i <= e)
}

// ---------- 读取(状态展示) ----------

/// 某域名当前实际生效的 IP(hosts 按文件顺序首条命中)
pub fn effective_ip(hosts_content: &str, domain: &str) -> Option<String> {
    for line in hosts_content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut it = t.split_whitespace();
        if let (Some(ip), Some(dom)) = (it.next(), it.next()) {
            if dom.eq_ignore_ascii_case(domain) {
                return Some(ip.to_string());
            }
        }
    }
    None
}

// ---------- 落盘(备份 → 写入 → 校验 → 刷新 DNS;失败回滚) ----------

/// 应用写入任务(带单实例互斥锁,GUI/--auto/helper 共用入口)
pub fn apply_job(job: &WriteJob) -> WriteOutcome {
    with_lock(|| apply_job_inner(&hosts_path(), job))
}

/// 互斥锁:防止 GUI 模式与 --auto 模式同时写 hosts
fn with_lock(f: impl FnOnce() -> WriteOutcome) -> WriteOutcome {
    let lock = crate::config::app_dir().join("hosts.lock");
    let stale = fs::metadata(&lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|e| e > Duration::from_secs(60))
        .unwrap_or(true);
    if !stale {
        if let Ok(s) = fs::read_to_string(&lock) {
            if !s.trim().is_empty() {
                return WriteOutcome::fail("另一实例正在写入 hosts(hosts.lock 被占用)");
            }
        }
    }
    let _ = fs::write(&lock, std::process::id().to_string());
    let out = f();
    let _ = fs::remove_file(&lock);
    out
}

fn apply_job_inner(path: &Path, job: &WriteJob) -> WriteOutcome {
    let current = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return WriteOutcome::fail(format!("读取 hosts 失败: {e}")),
    };
    let plan = build_plan(&current, job);
    if !plan.changed {
        return WriteOutcome { ok: true, message: "hosts 无需变更".into(), diff: plan.diff };
    }

    // 备份:etc 目录一份 + 数据目录按时间戳一份
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup_dir = crate::config::app_dir().join("backups");
    let _ = fs::create_dir_all(&backup_dir);
    let etc_bak = path.with_extension("cf-hosts-gui.bak");
    if let Err(e) = fs::write(&etc_bak, &current) {
        return WriteOutcome::fail(format!("备份失败(etc): {e}"));
    }
    if let Err(e) = fs::write(backup_dir.join(format!("hosts-{ts}.bak")), &current) {
        return WriteOutcome::fail(format!("备份失败(appdata): {e}"));
    }

    // 写入(先清只读属性)
    if let Err(e) = write_hosts_file(path, &plan.new_content) {
        let _ = fs::write(path, &current);
        return WriteOutcome::fail(format!("写入 hosts 失败: {e}(已回滚)"));
    }

    // 校验:读回比对
    match fs::read_to_string(path) {
        Ok(back) if back == plan.new_content => {}
        Ok(_) => {
            let _ = fs::write(path, &current);
            return WriteOutcome::fail("写入后读回校验不一致,已回滚备份");
        }
        Err(e) => {
            let _ = fs::write(path, &current);
            return WriteOutcome::fail(format!("校验读取失败: {e}(已回滚)"));
        }
    }

    // 刷新 DNS 缓存
    if let Err(e) = flush_dns() {
        return WriteOutcome {
            ok: true,
            message: format!("已写入,但刷新 DNS 失败: {e}(请手动执行 ipconfig /flushdns)"),
            diff: plan.diff,
        };
    }
    WriteOutcome {
        ok: true,
        message: "已写入 hosts 并刷新系统 DNS(浏览器自带缓存约 1 分钟内自行失效)".into(),
        diff: plan.diff,
    }
}

fn write_hosts_file(path: &Path, content: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // hosts 可能带只读属性,先清掉
        let _ = run_hidden("attrib", &["-r", &path.to_string_lossy()]);
    }
    fs::write(path, content)
}
