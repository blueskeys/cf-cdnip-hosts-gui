//! egui 界面(v4 设计系统版,依据 design-system/cf-hosts-gui/MASTER.md):
//!   - 语义色 token(深/浅双主题):深海军蓝分层(bg/panel/card)+ 状态绿 accent
//!   - 矢量单线图标(16px/1.6px 描边,同族),替代 emoji 图标
//!   - 卡片化布局 + 8px 间距节奏 + 统一字号阶梯(13/14/15/18/20,YaHei UI 子集)
//!   - 概览(两行)→ 动作条(按钮+动画进度)→ 胶囊导航 → 页面卡片
//! 测速与 hosts 写入都在后台线程执行,经 mpsc 事件驱动 UI 重绘,不阻塞界面。

use crate::config::{Config, DomainEntry};
use crate::hosts::{WriteJob, WriteMode, WriteOutcome};
use crate::logger::Logger;
use crate::privilege;
use crate::speedtest::{self, SpeedtestEvent, SpeedtestResult};
use crate::{bindings, meta_line};
use eframe::egui;
use eframe::egui::{Align, Color32, Layout, RichText};
use egui_extras::{Column, TableBuilder};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

// ---------- 设计 token(Glassmorphism 深色科技风;双主题) ----------

/// 语义色板:全部颜色经此取用,禁止散落硬编码(pro-rules: token-driven theming)
#[derive(Clone, Copy)]
struct Palette {
    /// 最深层(窗口底/控制台)
    deep: Color32,
    /// 面板层(顶部/导航)
    panel: Color32,
    /// 卡片层
    card: Color32,
    /// 卡片内斑马纹
    card_alt: Color32,
    /// 普通描边
    border: Color32,
    /// 强描边(按钮边)
    border_strong: Color32,
    /// 主文字
    fg: Color32,
    /// 弱文字
    muted_fg: Color32,
    /// 强调色(按钮填充/进度)
    accent: Color32,
    /// 强调色上的文字
    on_accent: Color32,
    /// 状态:成功(文字用,深色下用亮绿)
    ok: Color32,
    /// 状态:警告
    warn: Color32,
    /// 状态:错误
    err: Color32,
}

fn pal(ui: &egui::Ui) -> Palette {
    palette(ui.visuals().dark_mode)
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            deep: Color32::from_rgb(0x0B, 0x12, 0x20),
            panel: Color32::from_rgb(0x0F, 0x17, 0x2A),
            card: Color32::from_rgb(0x1B, 0x23, 0x36),
            card_alt: Color32::from_rgb(0x18, 0x20, 0x33),
            border: Color32::from_rgb(0x2E, 0x3A, 0x52),
            border_strong: Color32::from_rgb(0x47, 0x55, 0x69),
            fg: Color32::from_rgb(0xF8, 0xFA, 0xFC),
            muted_fg: Color32::from_rgb(0x94, 0xA3, 0xB8),
            accent: Color32::from_rgb(0x22, 0xC5, 0x5E),
            on_accent: Color32::from_rgb(0x0F, 0x17, 0x2A),
            ok: Color32::from_rgb(0x4A, 0xDE, 0x80),
            warn: Color32::from_rgb(0xFB, 0xBF, 0x24),
            err: Color32::from_rgb(0xF8, 0x71, 0x71),
        }
    } else {
        Palette {
            deep: Color32::from_rgb(0xE2, 0xE8, 0xF0),
            panel: Color32::from_rgb(0xF8, 0xFA, 0xFC),
            card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            card_alt: Color32::from_rgb(0xF4, 0xF7, 0xFB),
            border: Color32::from_rgb(0xE2, 0xE8, 0xF0),
            border_strong: Color32::from_rgb(0xCB, 0xD5, 0xE1),
            fg: Color32::from_rgb(0x0F, 0x17, 0x2A),
            muted_fg: Color32::from_rgb(0x64, 0x74, 0x8B),
            accent: Color32::from_rgb(0x16, 0xA3, 0x4A),
            on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            ok: Color32::from_rgb(0x15, 0x80, 0x3D),
            warn: Color32::from_rgb(0xB4, 0x53, 0x09),
            err: Color32::from_rgb(0xDC, 0x26, 0x26),
        }
    }
}

/// 两色线性混合(用于选中态底色、胶囊底色)
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| -> u8 { (x as f32 * (1.0 - t) + y as f32 * t).round() as u8 };
    Color32::from_rgb(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
    )
}

/// 后台线程 → UI 的事件
enum AppEvent {
    Speedtest(SpeedtestEvent),
    WriteDone(WriteOutcome),
    /// Cloudflare DNS 发布/验证完成;附带解析出的 zone_id(用于缓存)
    CfDnsDone(WriteOutcome, Option<String>),
    /// 当前 A 记录查询完成(Ok(None) = 记录不存在)
    CfRecordFetched(Result<Option<crate::cfapi::CfRecord>, String>),
}

/// 设置页「当前记录」行的展示状态
enum CfRecordView {
    /// 未查询过(启动初始态)
    Unknown,
    /// 查询成功但记录不存在
    Missing,
    /// 查询成功:Cloudflare 侧现值
    Live(crate::cfapi::CfRecord),
    /// 查询失败(Token 过期/网络等)
    Failed(String),
}

/// "%Y-%m-%d %H:%M" → 本地时区 DateTime(用于从 last_run.at 恢复定时器基准)
fn parse_local_dt(s: &str) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
        .ok()
        .and_then(|dt| chrono::Local.from_local_datetime(&dt).single())
}

/// UI 收集的待执行动作(点击后在面板闭包外统一处理,避开借用冲突)
enum Act {
    StartTest { auto_apply: bool },
    ApplyNow,
    RestoreAll,
    AddDomain,
    RemoveDomain(usize),
    PinIp(usize, String),
    /// 选定并写入 hosts,同时强制更新 Cloudflare DNS(不受 enabled 开关限制)
    PinIpDns(usize, String),
    ClearPin(usize),
    SaveConfig,
    VerifyCfDns,
    UpdateCfDns,
    /// 查询发布域名当前 A 记录(设置页「查看记录」)
    FetchCfRecord,
    ToggleTheme,
}

/// dispatch_write 的 DNS 发布模式
enum DnsMode {
    /// 不发布(还原操作)
    Off,
    /// 启用动态发布时才发布(常规绑定写入)
    Auto,
    /// 强制发布(行内「更新DNS」按钮,只需 Token/域名已配置)
    Force,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Domains,
    Results,
    Settings,
    Logs,
}

/// 测速结果页内嵌页签:结果列表 / 运行输出(各占整页高度)
#[derive(PartialEq, Clone, Copy)]
enum ResultsTab {
    List,
    Output,
}

/// cfst 阶段进度(phase 0=延迟 1=下载;note 如 "可用: 55")
struct ProgInfo {
    phase: u8,
    done: u32,
    total: u32,
    note: String,
}

pub struct CfApp {
    cfg: Config,
    log: Logger,
    tab: Tab,
    busy: Option<String>,
    stage: String,
    output_lines: Vec<String>,
    results: Vec<SpeedtestResult>,
    auto_apply: bool,
    test_start: Option<Instant>,
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    /// (域名, hosts 中当前生效 IP)
    hosts_effective: Vec<(String, Option<String>)>,
    new_domain: String,
    bind_target: usize,
    status_msg: String,
    cfst_found: Option<String>,
    ip_found: Option<String>,
    /// 内置定时器:上次自动执行时刻(chrono 本地时间,跨重启从 last_run.at 恢复)
    last_auto_at: Option<chrono::DateTime<chrono::Local>>,    /// 设置页的 cfst 路径编辑镜像(空 = 自动探测)
    ui_cfst_path: String,
    /// 设置页的 Cloudflare API Token 编辑镜像(密码框;空 = 未配置)
    ui_cf_token: String,
    /// 设置页「当前记录」行:Cloudflare 侧 A 记录现值回显
    cf_record: CfRecordView,
    /// 测速结果页内嵌页签
    results_tab: ResultsTab,
    /// 空闲计时起点与工作集修剪标记(空闲内存优化)
    idle_since: Instant,
    /// 测速阶段进度(来自 cfst 进度条解析);None = 无进度数据(显示活跃动画)
    prog: Option<ProgInfo>,
    ws_trimmed: bool,
}

impl CfApp {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let mut cfg = Config::load();
        let cfst_found = speedtest::resolve_cfst(&cfg).map(|p| p.display().to_string());
        let ip_found = speedtest::resolve_ip_file(&cfg).map(|p| p.display().to_string());
        let ui_cfst_path = cfg.speedtest.cfst_path.clone().unwrap_or_default();
        let ui_cf_token = cfg.cf_dns.api_token.clone();

        // 启动时从磁盘恢复上次测速结果(重启 GUI 不丢失,可直接「写入 hosts」)
        let mut restored = 0usize;
        let mut results: Vec<SpeedtestResult> = Vec::new();
        let csv = speedtest::result_csv_path();
        if let Ok(mut v) = speedtest::parse_csv(&csv) {
            if !v.is_empty() {
                v.truncate(cfg.speedtest.result_count.max(1) as usize);
                restored = v.len();
                let at = std::fs::metadata(&csv).ok().and_then(|m| m.modified().ok()).map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.format("%Y-%m-%d %H:%M").to_string()
                });
                let best = v[0].clone();
                cfg.last_run = crate::config::LastRun {
                    best_ip: Some(best.ip.clone()),
                    best_latency_ms: Some(best.avg_latency_ms),
                    best_speed_mb: Some(best.speed_mb),
                    best_region: Some(best.region.clone()),
                    result_count: restored,
                    at,
                };
                let _ = cfg.save();
                results = v;
            }
        }

        // 内置定时器:以上次成功运行时间为基准恢复周期(重启不重置)
        let last_auto_at = cfg.last_run.at.as_deref().and_then(parse_local_dt);
        let mut app = Self {
            cfg,
            log: Logger::new(),
            tab: Tab::Domains,
            busy: None,
            stage: String::new(),
            output_lines: Vec::new(),
            results,
            auto_apply: false,
            test_start: None,
            tx,
            rx,
            hosts_effective: Vec::new(),
            new_domain: String::new(),
            bind_target: 0,
            status_msg: String::new(),
            cfst_found,
            ip_found,
            last_auto_at,
            ui_cfst_path,
            ui_cf_token,
            cf_record: CfRecordView::Unknown,
            results_tab: ResultsTab::List,
            idle_since: Instant::now(),
            ws_trimmed: false,
            prog: None,
        };
        app.refresh_hosts_status();
        app.log.info("CF Hosts GUI 启动");
        if restored > 0 {
            app.log.info(format!(
                "已恢复上次测速结果 {restored} 条(最优 {},可直接「写入 hosts」)",
                app.results[0].ip
            ));
        }
        app
    }

    fn refresh_hosts_status(&mut self) {
        let content = std::fs::read_to_string(crate::hosts::hosts_path()).unwrap_or_default();
        self.hosts_effective = self
            .cfg
            .domains
            .iter()
            .map(|d| (d.name.clone(), crate::hosts::effective_ip(&content, &d.name)))
            .collect();
    }

    // ---------- 事件泵 ----------

    fn pump_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                AppEvent::Speedtest(SpeedtestEvent::Stage(s)) => {
                    self.stage.clone_from(&s);
                    self.log.info(format!("[cfst] {s}"));
                }
                AppEvent::Speedtest(SpeedtestEvent::Progress { phase, done, total, note }) => {
                    if let Some(pr) = self.prog.as_mut() {
                        pr.phase = phase;
                        pr.done = done;
                        pr.total = total;
                        pr.note = note;
                    } else {
                        self.prog = Some(ProgInfo { phase, done, total, note });
                    }
                }
                AppEvent::Speedtest(SpeedtestEvent::Line(l)) => {
                    self.output_lines.push(l);
                    if self.output_lines.len() > 400 {
                        let d = self.output_lines.len() - 400;
                        self.output_lines.drain(0..d);
                    }
                }
                AppEvent::Speedtest(SpeedtestEvent::Done(v)) => {
                    self.busy = None;
                    self.test_start = None;
                    self.prog = None;
                    // 测完自动切回「结果列表」
                    self.results_tab = ResultsTab::List;
                    let best = v[0].clone();
                    self.results = v;
                    self.cfg.last_run = crate::config::LastRun {
                        best_ip: Some(best.ip.clone()),
                        best_latency_ms: Some(best.avg_latency_ms),
                        best_speed_mb: Some(best.speed_mb),
                        best_region: Some(best.region.clone()),
                        result_count: self.results.len(),
                        at: Some(chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()),
                    };
                    let _ = self.cfg.save();
                    self.log.info(format!(
                        "测速完成:最优 {} ({:.1}ms / {:.2}MB/s {})",
                        best.ip, best.avg_latency_ms, best.speed_mb, best.region
                    ));
                    self.status_msg = format!(
                        "测速完成:最优 {}({:.0}ms / {:.1}MB/s {})",
                        best.ip, best.avg_latency_ms, best.speed_mb, best.region
                    );
                    if self.auto_apply {
                        self.apply_now(ctx);
                    }
                }
                AppEvent::Speedtest(SpeedtestEvent::Failed(e)) => {
                    self.busy = None;
                    self.test_start = None;
                    self.prog = None;
                    self.log.error(format!("测速失败: {e}"));
                    self.status_msg = format!("测速失败:{e}");
                }
                AppEvent::WriteDone(o) => {
                    self.busy = None;
                    self.refresh_hosts_status();
                    if o.ok {
                        self.log.info(&o.message);
                    } else {
                        self.log.error(&o.message);
                    }
                    self.status_msg = if o.ok {
                        format!("✓ {}", o.message)
                    } else {
                        format!("✗ {}", o.message)
                    };
                }
                AppEvent::CfDnsDone(o, zone) => {
                    self.busy = None;
                    // 缓存 zone_id,后续发布少 1-2 次 API 查询
                    if let Some(z) = zone {
                        if self.cfg.cf_dns.zone_id.as_deref() != Some(z.as_str()) {
                            self.cfg.cf_dns.zone_id = Some(z);
                            let _ = self.cfg.save();
                        }
                    }
                    if o.ok {
                        self.log.info(&o.message);
                        // 发布/验证成功后回查一次,「当前记录」行保持与 Cloudflare 一致
                        self.fetch_cf_record_async(ctx);
                    } else {
                        self.log.error(&o.message);
                    }
                    self.status_msg = if o.ok {
                        format!("✓ {}", o.message)
                    } else {
                        format!("✗ {}", o.message)
                    };
                }
                AppEvent::CfRecordFetched(r) => match r {
                    Ok(rec) => {
                        self.cf_record =
                            if let Some(rec) = rec { CfRecordView::Live(rec) } else { CfRecordView::Missing };
                    }
                    Err(e) => {
                        self.cf_record = CfRecordView::Failed(e.clone());
                        self.status_msg = format!("✗ 查询记录失败:{e}");
                    }
                },
            }
        }
    }

    /// 内置定时器:到点自动执行「测速 → 写 hosts → 发布 Cloudflare DNS」完整链路。
    /// 程序运行期间生效,跨平台,不依赖系统计划任务;busy 时顺延到空闲。
    fn maybe_auto_run(&mut self, ctx: &egui::Context) {
        if !self.cfg.schedule.enabled || self.busy.is_some() || self.cfg.domains.is_empty() {
            return;
        }
        let interval =
            chrono::Duration::minutes(self.cfg.schedule.interval_minutes.max(1) as i64);
        let due = match self.last_auto_at {
            Some(t) => chrono::Local::now() - t >= interval,
            None => true,
        };
        if due {
            self.last_auto_at = Some(chrono::Local::now());
            let next_min = self.cfg.schedule.interval_minutes;
            self.log
                .info(format!("内置定时器触发:自动测速并更新(每 {next_min} 分钟)"));
            self.start_speedtest(ctx, true);
        }
    }

    // ---------- 动作处理 ----------

    fn handle(&mut self, ctx: &egui::Context, act: Act) {
        match act {
            Act::StartTest { auto_apply } => self.start_speedtest(ctx, auto_apply),
            Act::ApplyNow => self.apply_now(ctx),
            Act::RestoreAll => {
                if self.busy.is_some() {
                    return;
                }
                let job =
                    WriteJob { mode: WriteMode::RemoveAll, entries: vec![], meta: String::new() };
                self.log.info("请求还原:移除管理区块");
                self.dispatch_write(ctx, job, "还原 hosts", DnsMode::Off);
            }
            Act::FetchCfRecord => {
                self.sync_cf_token();
                if self.cfg.cf_dns.zone_id.is_none() {
                    self.status_msg = "请先「验证连接」定位 Zone".into();
                    return;
                }
                self.fetch_cf_record_async(ctx);
            }
            Act::ToggleTheme => {
                self.cfg.dark_mode = !self.cfg.dark_mode;
                let _ = self.cfg.save();
                apply_visuals(ctx, self.cfg.dark_mode);
                self.log
                    .info(format!("主题切换为{}", if self.cfg.dark_mode { "深色" } else { "浅色" }));
            }
            Act::AddDomain => {
                let name = self.new_domain.trim().to_string();
                if name.is_empty() || name.contains(char::is_whitespace) || !name.contains('.') {
                    self.status_msg = "域名格式不正确(应如 cc.example.com)".into();
                    return;
                }
                if self.cfg.domains.iter().any(|d| d.name.eq_ignore_ascii_case(&name)) {
                    self.status_msg = format!("域名已存在:{name}");
                    return;
                }
                self.cfg.domains.push(DomainEntry { name, pin_ip: None });
                self.new_domain.clear();
                let _ = self.cfg.save();
                self.refresh_hosts_status();
                self.status_msg = "域名已添加;测速并写入后生效".into();
                self.log.info(&self.status_msg);
            }
            Act::RemoveDomain(i) => {
                if let Some(d) = self.cfg.domains.get(i).cloned() {
                    self.cfg.domains.remove(i);
                    let _ = self.cfg.save();
                    self.refresh_hosts_status();
                    if self.bind_target >= self.cfg.domains.len() {
                        self.bind_target = self.cfg.domains.len().saturating_sub(1);
                    }
                    self.status_msg = format!("已移除 {};重新写入 hosts 可清掉它的记录", d.name);
                    self.log.info(&self.status_msg);
                }
            }
            Act::PinIp(i, ip) => {
                if let Some(d) = self.cfg.domains.get_mut(i) {
                    d.pin_ip = Some(ip);
                }
                let _ = self.cfg.save();
                let job = WriteJob {
                    mode: WriteMode::Replace,
                    entries: bindings(&self.cfg, &self.results),
                    meta: meta_line(&self.results[0]),
                };
                self.dispatch_write(ctx, job, "写入Hosts", DnsMode::Auto);
                self.status_msg = "已选定 IP,正在写入 hosts…".into();
                self.log.info(&self.status_msg);
            }
            Act::PinIpDns(i, ip) => {
                let token = self.cfg.cf_dns.api_token.trim().to_string();
                let domain =
                    self.cfg.cf_dns.domain.trim().trim_end_matches('.').to_string();
                if token.is_empty() || domain.is_empty() {
                    self.status_msg =
                        "「更新DNS」需先在设置中配置 Cloudflare Token 与发布域名".into();
                    self.log.error(&self.status_msg);
                    return;
                }
                if let Some(d) = self.cfg.domains.get_mut(i) {
                    d.pin_ip = Some(ip);
                }
                let _ = self.cfg.save();
                let job = WriteJob {
                    mode: WriteMode::Replace,
                    entries: bindings(&self.cfg, &self.results),
                    meta: meta_line(&self.results[0]),
                };
                self.dispatch_write(ctx, job, "更新DNS", DnsMode::Force);
                self.status_msg = "已选定 IP,写入 hosts 并更新 Cloudflare DNS…".into();
                self.log.info(&self.status_msg);
            }
            Act::ClearPin(i) => {
                if let Some(d) = self.cfg.domains.get_mut(i) {
                    d.pin_ip = None;
                }
                let _ = self.cfg.save();
                if self.results.is_empty() {
                    self.status_msg = "已恢复自动跟随第一名;写入后生效".into();
                    self.log.info(&self.status_msg);
                } else {
                    let job = WriteJob {
                        mode: WriteMode::Replace,
                        entries: bindings(&self.cfg, &self.results),
                        meta: meta_line(&self.results[0]),
                    };
                    self.dispatch_write(ctx, job, "清除选定", DnsMode::Auto);
                    self.status_msg = "已恢复自动跟随第一名,正在写入 hosts…".into();
                    self.log.info(&self.status_msg);
                }
            }
            Act::SaveConfig => {
                let p = self.ui_cfst_path.trim().to_string();
                self.cfg.speedtest.cfst_path = if p.is_empty() { None } else { Some(p) };
                let t = self.ui_cf_token.trim().to_string();
                if self.cfg.cf_dns.api_token != t {
                    self.cfg.cf_dns.api_token = t;
                    // Token 变更后 zone 缓存可能失效,清空待重新验证
                    self.cfg.cf_dns.zone_id = None;
                }
                let _ = self.cfg.save();
                self.cfst_found =
                    speedtest::resolve_cfst(&self.cfg).map(|p| p.display().to_string());
                self.ip_found =
                    speedtest::resolve_ip_file(&self.cfg).map(|p| p.display().to_string());
                self.status_msg = "设置已保存".into();
                self.log.info("设置已保存");
            }
            Act::VerifyCfDns => {
                if self.busy.is_some() {
                    return;
                }
                // 点「验证连接」即同步保存 Token,避免填了没保存、重启后丢失
                self.sync_cf_token();
                let token = self.ui_cf_token.trim().to_string();
                let domain =
                    self.cfg.cf_dns.domain.trim().trim_end_matches('.').to_string();
                if token.is_empty() || domain.is_empty() {
                    self.status_msg = "请先填写 Cloudflare API Token 与发布域名".into();
                    return;
                }
                self.busy = Some("验证 Cloudflare 连接".into());
                let tx = self.tx.clone();
                let ectx = ctx.clone();
                std::thread::spawn(move || {
                    let (zone, outcome) =
                        match crate::cfapi::verify(&token)
                            .and_then(|_| crate::cfapi::resolve_zone(&token, &domain))
                        {
                            Ok(z) => (
                                Some(z),
                                WriteOutcome {
                                    ok: true,
                                    message: "Cloudflare 连接正常:Token 有效,已定位发布域名所在 Zone"
                                        .into(),
                                    diff: vec![],
                                },
                            ),
                            Err(e) => (None, WriteOutcome::fail(e)),
                        };
                    let _ = tx.send(AppEvent::CfDnsDone(outcome, zone));
                    ectx.request_repaint();
                });
            }
            Act::UpdateCfDns => {
                if self.busy.is_some() {
                    return;
                }
                self.sync_cf_token();
                let token = self.ui_cf_token.trim().to_string();
                let domain =
                    self.cfg.cf_dns.domain.trim().trim_end_matches('.').to_string();
                if token.is_empty() || domain.is_empty() {
                    self.status_msg = "请先填写 Cloudflare API Token 与发布域名".into();
                    return;
                }
                let Some(best) = self.results.first().map(|r| r.ip.clone()) else {
                    self.status_msg = "没有测速结果,请先测速".into();
                    return;
                };
                self.busy = Some("更新 Cloudflare DNS".into());
                let zone_cached = self.cfg.cf_dns.zone_id.clone();
                let tx = self.tx.clone();
                let ectx = ctx.clone();
                std::thread::spawn(move || {
                    let (zone, outcome) = match zone_cached {
                        Some(z) => {
                            let d = crate::cfapi::upsert_record(&token, &z, &domain, &best);
                            (Some(z), d)
                        }
                        None => match crate::cfapi::resolve_zone(&token, &domain) {
                            Ok(z) => {
                                let d = crate::cfapi::upsert_record(&token, &z, &domain, &best);
                                (Some(z), d)
                            }
                            Err(e) => (
                                None,
                                WriteOutcome::fail(format!(
                                    "Cloudflare DNS 更新失败:定位 Zone 失败:{e}"
                                )),
                            ),
                        },
                    };
                    let _ = tx.send(AppEvent::CfDnsDone(outcome, zone));
                    ectx.request_repaint();
                });
            }
        }
    }

    /// 把设置页 Token 输入框的值同步回配置并落盘(值有变时清空 zone 缓存)
    fn sync_cf_token(&mut self) {
        let t = self.ui_cf_token.trim().to_string();
        if self.cfg.cf_dns.api_token != t {
            self.cfg.cf_dns.api_token = t;
            self.cfg.cf_dns.zone_id = None;
        }
        let _ = self.cfg.save();
    }

    /// 后台只读查询发布域名当前 A 记录(不占 busy 闸门,结果经 CfRecordFetched 回流)
    fn fetch_cf_record_async(&self, ctx: &egui::Context) {
        let token = self.cfg.cf_dns.api_token.trim().to_string();
        let domain = self.cfg.cf_dns.domain.trim().trim_end_matches('.').to_string();
        let Some(zone) = self.cfg.cf_dns.zone_id.clone() else { return };
        if token.is_empty() || domain.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        let ectx = ctx.clone();
        std::thread::spawn(move || {
            let r = crate::cfapi::fetch_record(&token, &zone, &domain);
            let _ = tx.send(AppEvent::CfRecordFetched(r));
            ectx.request_repaint();
        });
    }

    fn start_speedtest(&mut self, ctx: &egui::Context, auto_apply: bool) {
        if self.busy.is_some() {
            return;
        }
        self.auto_apply = auto_apply;
        self.busy = Some("测速中".into());
        self.test_start = Some(Instant::now());
        self.prog = None;
        self.stage = "准备启动 cfst…".into();
        self.output_lines.clear();
        // 开始测速自动切到「运行输出」看实时日志
        self.results_tab = ResultsTab::Output;
        let _ = self.cfg.save();
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();
        let ectx = ctx.clone();
        self.log.info("开始测速");
        speedtest::spawn_speedtest(&cfg, move |ev| {
            let _ = tx.send(AppEvent::Speedtest(ev));
            ectx.request_repaint();
        });
    }

    fn apply_now(&mut self, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        if self.results.is_empty() {
            self.status_msg = "没有可用的测速结果,请先测速".into();
            return;
        }
        if self.cfg.domains.is_empty() {
            self.status_msg = "域名列表为空".into();
            return;
        }
        let job = WriteJob {
            mode: WriteMode::Replace,
            entries: bindings(&self.cfg, &self.results),
            meta: meta_line(&self.results[0]),
        };
        self.dispatch_write(ctx, job, "写入 hosts", DnsMode::Auto);
    }

    /// `dns_mode`:hosts 写入成功后是否顺带把最优 IP 发布到 Cloudflare DNS
    /// (还原传 Off;常规绑定写入传 Auto,受「启用动态发布」开关控制;
    ///  行内「更新DNS」传 Force,只要 Token/域名已配置就发布)
    fn dispatch_write(
        &mut self,
        ctx: &egui::Context,
        job: WriteJob,
        label: &str,
        dns_mode: DnsMode,
    ) {
        self.busy = Some(label.to_string());
        let want_dns = match dns_mode {
            DnsMode::Off => false,
            DnsMode::Auto => self.cfg.cf_dns.enabled,
            DnsMode::Force => true,
        };
        // DNS 发布任务快照(跨线程只读;未启用或未配置则为 None)
        let cf_job: Option<(String, String, Option<String>, String)> = if want_dns {
                let c = &self.cfg.cf_dns;
                let token = c.api_token.trim().to_string();
                let domain = c.domain.trim().trim_end_matches('.').to_string();
                if token.is_empty() || domain.is_empty() {
                    None
                } else {
                    self.results
                        .first()
                        .map(|r| (token, domain, c.zone_id.clone(), r.ip.clone()))
                }
            } else {
                None
            };
        let tx = self.tx.clone();
        let ectx = ctx.clone();
        let log = self.log.clone();
        std::thread::spawn(move || {
            let outcome = if privilege::is_elevated() {
                crate::hosts::apply_job(&job)
            } else {
                privilege::elevate_write_hosts(&job).unwrap_or_else(|e| WriteOutcome::fail(e))
            };
            if outcome.ok {
                for d in &outcome.diff {
                    log.info(d);
                }
                log.info(&outcome.message);
            } else {
                log.error(format!("hosts 写入失败: {}", outcome.message));
            }
            let _ = tx.send(AppEvent::WriteDone(outcome.clone()));
            ectx.request_repaint();

            // hosts 成功后,把当前最优 IP 发布为灰云 A 记录(供外部 dig 查询)
            if outcome.ok {
                if let Some((token, domain, zone_id, ip)) = cf_job {
                    let (zone, dns) = match zone_id {
                        Some(z) => {
                            let d = crate::cfapi::upsert_record(&token, &z, &domain, &ip);
                            (Some(z), d)
                        }
                        None => match crate::cfapi::resolve_zone(&token, &domain) {
                            Ok(z) => {
                                let d = crate::cfapi::upsert_record(&token, &z, &domain, &ip);
                                (Some(z), d)
                            }
                            Err(e) => (
                                None,
                                WriteOutcome::fail(format!(
                                    "Cloudflare DNS 发布失败:定位 Zone 失败:{e}"
                                )),
                            ),
                        },
                    };
                    if dns.ok {
                        for d in &dns.diff {
                            log.info(d);
                        }
                    } else {
                        log.error(&dns.message);
                    }
                    let _ = tx.send(AppEvent::CfDnsDone(dns, zone));
                    ectx.request_repaint();
                }
            }
        });
    }

    // ---------- UI:整体布局 ----------

    pub fn ui_update(&mut self, ctx: &egui::Context) {
        self.pump_events(ctx);
        let mut acts: Vec<Act> = Vec::new();

        // 隐藏到托盘期间:不构建 UI(窗口隐藏时无 vsync,Spinner/进度条等动画
        // 控件会请求无限帧率重绘,把 CPU 打满);后台逻辑(事件泵/定时器)照常跑。
        #[cfg(windows)]
        let tray_hidden = {
            // 仍收到输入事件说明窗口实际可见:兜底清标志,防状态失步后界面空白
            let got_input = ctx.input(|i| !i.events.is_empty());
            if got_input {
                crate::tray::HIDDEN.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            crate::tray::HIDDEN.load(std::sync::atomic::Ordering::SeqCst) && !got_input
        };
        #[cfg(not(windows))]
        let tray_hidden = false;

        if !tray_hidden {
            egui::TopBottomPanel::top("overview").show(ctx, |ui| self.ui_overview(ui));
            egui::TopBottomPanel::top("actions").show(ctx, |ui| self.ui_actions(ui, &mut acts));
            egui::SidePanel::left("nav")
                .resizable(false)
                .default_width(148.0)
                .show(ctx, |ui| self.ui_nav(ui, &mut acts));
            egui::CentralPanel::default().show(ctx, |ui| match self.tab {
                Tab::Domains => self.ui_domains(ui, &mut acts),
                Tab::Results => self.ui_results(ui, &mut acts),
                Tab::Settings => self.ui_settings(ui, &mut acts),
                Tab::Logs => self.ui_logs(ui),
            });
        }
        // 隐藏期间按固定节奏醒着即可(线程事件会另行唤醒)
        else {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }

        for a in acts {
            self.handle(ctx, a);
        }
        // 内置定时器:检查是否到点(用户操作优先生效,忙时顺延)
        self.maybe_auto_run(ctx);

        // 关窗被 tray 子类化 WndProc 拦截并隐藏到托盘;这里只负责记录一次日志
        #[cfg(windows)]
        if crate::tray::HIDDEN_ONCE.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.log.info("已最小化到托盘,后台继续定时更新;托盘左键/菜单可打开,右键菜单可退出");
        }

        if self.busy.is_some() {
            self.idle_since = Instant::now();
            self.ws_trimmed = false;
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        } else if !self.ws_trimmed {
            if self.idle_since.elapsed().as_secs() >= 10 {
                // 空闲 10 秒:修剪工作集,把内存页面还给系统(交互时自动换回)
                trim_working_set();
                self.ws_trimmed = true;
                self.log.info("已进入空闲:修剪工作集,降低任务管理器内存占用");
            } else {
                // egui 空闲时不再重绘,update 不会被调;预约一帧在修剪时刻醒来
                let left = 10 - self.idle_since.elapsed().as_secs();
                ctx.request_repaint_after(std::time::Duration::from_secs(left + 1));
            }
        }

        // 内置定时器启用时:预约一帧在到点时刻醒来(空闲下 update 不会被调)
        if self.cfg.schedule.enabled && self.busy.is_none() {
            let interval_secs = 60u64 * self.cfg.schedule.interval_minutes.max(1) as u64;
            let elapsed = self
                .last_auto_at
                .map(|t| (chrono::Local::now() - t).num_seconds().max(0) as u64)
                .unwrap_or(0);
            let remaining = interval_secs.saturating_sub(elapsed);
            ctx.request_repaint_after(std::time::Duration::from_secs(remaining + 1));
        }
    }

    // ---------- UI:概览(两行分组) ----------

    fn ui_overview(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        ui.add_space(8.0);
        // 第一行:当前最优(等宽大字)+ 指标 + 右侧上次更新
        ui.horizontal(|ui| {
            ui.label(RichText::new("当前最优").small().color(p.muted_fg));
            let lr = &self.cfg.last_run;
            match (&lr.best_ip, &lr.best_latency_ms, &lr.best_speed_mb, &lr.best_region) {
                (Some(ip), Some(ms), Some(mb), Some(rg)) => {
                    ui.label(
                        RichText::new(ip.as_str())
                            .monospace()
                            .size(20.0)
                            .strong()
                            .color(p.ok),
                    );
                    ui.label(
                        RichText::new(format!("{ms:.0}ms · {mb:.1}MB/s · {rg}"))
                            .small()
                            .color(p.muted_fg),
                    );
                }
                _ => {
                    ui.label(RichText::new("尚未测速").weak());
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(at) = &lr.at {
                    ui.label(RichText::new(format!("上次更新 {at}")).small().color(p.muted_fg));
                }
            });
        });
        ui.add_space(4.0);
    }

    // ---------- UI:动作条(按钮与进度同排) ----------

    fn ui_actions(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = pal(ui);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let idle = self.busy.is_none();

            // 主操作:accent 实底 + 矢量闪电
            let b1 = styled_button(
                ui,
                "一键测速并更新",
                Some(NavIcon::Gauge),
                BtnStyle::Primary,
                BtnSize::Normal,
                idle,
                if idle { "" } else { "测速进行中…" },
            );
            if b1.clicked() {
                acts.push(Act::StartTest { auto_apply: true });
            }
            ui.add_space(6.0);

            // 次操作:幽灵样式(描边透明底)
            let b2 = styled_button(ui, "仅测速", None, BtnStyle::Ghost, BtnSize::Normal, idle, "");
            if b2.clicked() {
                acts.push(Act::StartTest { auto_apply: false });
            }
            ui.add_space(6.0);
            let b3 = styled_button(
                ui,
                "写入 hosts",
                None,
                BtnStyle::Ghost,
                BtnSize::Normal,
                idle,
                "用当前测速结果更新 hosts 管理区块(弹一次 UAC)",
            );
            if b3.clicked() {
                acts.push(Act::ApplyNow);
            }
            ui.add_space(6.0);
            // 破坏性操作:平时低调,悬停警示红
            let b4 = styled_button(
                ui,
                "还原全部",
                None,
                BtnStyle::Danger,
                BtnSize::Normal,
                idle,
                "移除 cf-hosts-gui 的管理区块,不动 hosts 其他内容(弹一次 UAC)",
            );
            if b4.clicked() {
                acts.push(Act::RestoreAll);
            }

            // 右侧:进行中显示进度(触发与反馈相邻),空闲显示最近状态
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(b) = &self.busy {
                    ui.add(egui::Spinner::new());
                    let el = self
                        .test_start
                        .map(|t| format!(" · {}", fmt_elapsed(t.elapsed())))
                        .unwrap_or_default();
                    ui.label(RichText::new(format!("{b} · {}{el}", self.stage)).small().strong());
                } else if !self.status_msg.is_empty() {
                    let color = if self.status_msg.starts_with('✗') { p.err } else { p.muted_fg };
                    // 状态栏空间有限:超长截断(完整内容悬停可见/查日志)
                    let shown: String = if self.status_msg.chars().count() > 48 {
                        let s: String = self.status_msg.chars().take(48).collect();
                        format!("{s}…")
                    } else {
                        self.status_msg.clone()
                    };
                    let resp = ui.add(
                        egui::Label::new(RichText::new(shown).small().color(color))
                            .sense(egui::Sense::hover()),
                    );
                    resp.on_hover_text(self.status_msg.clone());
                }
            });
        });
        // 进行中:醒目进度条。有解析进度 → 当前阶段真实百分比 + 计数文字;
        // 无进度数据(如写入 hosts)→ 活跃动画 + 阶段文字
        if self.busy.is_some() {
            let (frac, text) = match &self.prog {
                Some(pr) if pr.total > 0 => {
                    let pct = (pr.done as f32 / pr.total as f32).clamp(0.0, 1.0);
                    let name = if pr.phase == 0 { "延迟测速" } else { "下载测速" };
                    let note =
                        if pr.note.is_empty() { String::new() } else { format!(" · {}", pr.note) };
                    (
                        pct.max(0.01),
                        format!("{name} {:.0}% · {}/{}{}", pct * 100.0, pr.done, pr.total, note),
                    )
                }
                _ => {
                    let t = ui.input(|i| i.time) as f32;
                    let frac =
                        (0.5 + 0.5 * (t * 2.0 * std::f32::consts::PI).sin()).clamp(0.02, 0.98);
                    (frac, self.stage.clone())
                }
            };
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_height(16.0)
                    .text(text)
                    .fill(p.accent),
            );
        }
        ui.add_space(4.0);
    }

    // ---------- UI:导航(矢量图标 + 胶囊选中态) ----------

    fn ui_nav(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = pal(ui);
        ui.add_space(10.0);
        // 品牌区:闪电 + 名称
        ui.horizontal(|ui| {
            draw_cloud(ui, p.accent, 22.0);
            ui.label(RichText::new("CF Hosts").size(16.0).strong());
        });
        ui.add_space(10.0);

        let items = [
            (Tab::Domains, NavIcon::Link, "域名"),
            (Tab::Results, NavIcon::Bolt, "测速"),
            (Tab::Settings, NavIcon::Gear, "设置"),
            (Tab::Logs, NavIcon::Lines, "日志"),
        ];
        for (t, icon, label) in items {
            let sel = self.tab == t;
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::click());
            let hovered = resp.hovered();
            if resp.clicked() {
                self.tab = t;
            }
            let bg = if sel {
                mix(p.panel, p.accent, 0.18)
            } else if hovered {
                mix(p.panel, p.card, 0.9)
            } else {
                Color32::TRANSPARENT
            };
            if bg != Color32::TRANSPARENT {
                ui.painter()
                    .rect(rect, egui::Rounding::same(8.0), bg, egui::Stroke::NONE);
            }
            if sel {
                // 左侧 2px 强调条
                let bar = egui::Rect::from_min_max(
                    egui::pos2(rect.left(), rect.top() + 8.0),
                    egui::pos2(rect.left() + 2.5, rect.bottom() - 8.0),
                );
                ui.painter().rect_filled(bar, egui::Rounding::same(2.0), p.accent);
            }
            let icon_color = if sel { p.accent } else { p.muted_fg };
            draw_nav_icon(ui, icon, rect.left_center() + egui::vec2(16.0, 0.0), icon_color);
            ui.painter().text(
                rect.left_center() + egui::vec2(36.0, 0.0),
                egui::Align2::LEFT_CENTER,
                label,
                egui::FontId::proportional(15.0),
                if sel { p.fg } else { p.muted_fg },
            );
            ui.add_space(2.0);
        }

        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            let el = if privilege::is_elevated() { "管理员权限" } else { "普通权限" };
            ui.label(RichText::new(el).small().color(p.muted_fg));
            ui.label(RichText::new(concat!("cf-hosts-gui v", env!("CARGO_PKG_VERSION"))).small().color(p.muted_fg));
            ui.add_space(2.0);
            let btn_label = if self.cfg.dark_mode { "切换浅色" } else { "切换深色" };
            if styled_button(ui, btn_label, None, BtnStyle::Ghost, BtnSize::Small, true, "")
                .clicked()
            {
                acts.push(Act::ToggleTheme);
            }
            ui.separator();
        });
    }

    // ---------- UI:域名管理 ----------

    fn ui_domains(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = pal(ui);
        page_header(ui, "域名", "DOMAINS", "程序只维护 hosts 中自己的区块,其他内容不动");

        card(ui, |ui| {
            // 添加行
            ui.horizontal(|ui| {
                ui.label("添加域名");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.new_domain)
                        .desired_width(280.0)
                        .hint_text("cc.example.com,回车添加"),
                );
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    acts.push(Act::AddDomain);
                }
                if styled_button(ui, "添加", None, BtnStyle::Primary, BtnSize::Small, true, "")
                    .clicked()
                {
                    acts.push(Act::AddDomain);
                }
            });
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(2.0);

            // 克隆渲染所需数据,避免闭包借用冲突
            let rows: Vec<(String, Option<String>, Option<String>)> = self
                .cfg
                .domains
                .iter()
                .map(|d| {
                    let eff = self
                        .hosts_effective
                        .iter()
                        .find(|(n, _)| n == &d.name)
                        .and_then(|(_, e)| e.clone());
                    (d.name.clone(), d.pin_ip.clone(), eff)
                })
                .collect();
            let best_ip = self.cfg.last_run.best_ip.clone();

            if rows.is_empty() {
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("暂无域名").size(15.0).strong());
                    ui.label(RichText::new("在上方输入框添加(如 example.com)").small().color(p.muted_fg));
                });
                ui.add_space(10.0);
                return;
            }

            table(ui, &[130.0, 130.0, 100.0, 160.0])
            .header(28.0, |mut header| {
                header.col(|ui| table_head(ui, p, "域名"));
                header.col(|ui| table_head(ui, p, "选定 IP"));
                header.col(|ui| table_head(ui, p, "当前生效"));
                header.col(|ui| table_head(ui, p, "状态"));
                header.col(|ui| table_head(ui, p, "操作"));
            })
            .body(|mut body| {
                for (i, (name, pin, eff)) in rows.iter().enumerate() {
                    body.row(30.0, |mut row| {
                        row.col(|ui| { ui.label(name); });
                        row.col(|ui| {
                            match pin {
                                Some(ip) => {
                                    ui.label(RichText::new(ip).monospace());
                                }
                                None => {
                                    ui.label(RichText::new("自动(第一名)").weak());
                                }
                            };
                        });
                        row.col(|ui| {
                            match eff {
                                Some(e) => {
                                    ui.label(RichText::new(e).monospace());
                                }
                                None => {
                                    ui.label(RichText::new("—").weak());
                                }
                            };
                        });
                        row.col(|ui| {
                            let expected = pin.clone().or_else(|| best_ip.clone());
                            let (txt, color, tip): (&str, Color32, String) =
                                match (eff, expected) {
                                    (Some(e), Some(x)) if e == &x => {
                                        ("✓ 生效", p.ok, "hosts 已指向期望 IP".into())
                                    }
                                    (Some(e), _) => (
                                        "⚠ 不一致",
                                        p.warn,
                                        format!("hosts 当前指向 {e};点上方「写入 hosts」更新"),
                                    ),
                                    _ => ("未写入", p.muted_fg, "尚未写入 hosts".into()),
                                };
                            ui.label(RichText::new(txt).color(color)).on_hover_text(tip);
                        });
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                if pin.is_some()
                                    && styled_button(
                                        ui,
                                        "清除选定",
                                        None,
                                        BtnStyle::Ghost,
                                        BtnSize::Small,
                                        true,
                                        "",
                                    )
                                    .clicked()
                                {
                                    acts.push(Act::ClearPin(i));
                                }
                                if styled_button(
                                    ui,
                                    "移除",
                                    None,
                                    BtnStyle::Danger,
                                    BtnSize::Small,
                                    true,
                                    "",
                                )
                                .clicked()
                                {
                                    acts.push(Act::RemoveDomain(i));
                                }
                            });
                        });
                    });
                }
            });
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new("提示:添加/移除域名后,点上方「把结果写入 hosts」生效;「清除选定」会立即写入。")
                .small()
                .color(p.muted_fg),
        );
    }

    // ---------- UI:测速结果 ----------

    fn ui_results(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = pal(ui);
        page_header(
            ui,
            "测速",
            "RESULTS",
            "cfst 按下载速度降序输出;灰色的行未通过下载测速(速度 0)",
        );

        // 内嵌双页签:结果列表 / 运行输出,各自独占整页高度
        ui.horizontal(|ui| {
            if seg_tab(ui, "结果列表", self.results_tab == ResultsTab::List) {
                self.results_tab = ResultsTab::List;
            }
            ui.add_space(4.0);
            let n = self.output_lines.len();
            let out_label = if n > 0 { format!("运行输出 ({n})") } else { "运行输出".to_string() };
            if seg_tab(ui, &out_label, self.results_tab == ResultsTab::Output) {
                self.results_tab = ResultsTab::Output;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new("测速时自动切到运行输出,完成后切回").small().color(p.muted_fg),
                );
            });
        });
        // 页签条基线:整宽细线,激活页签的下划线压在其上 → 经典 tab 观感
        let (line_rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 1.0),
            egui::Sense::hover(),
        );
        ui.painter().rect_filled(line_rect, egui::Rounding::same(0.0), p.border);
        ui.add_space(8.0);

        // ---- 运行输出页签:整页控制台 ----
        if self.results_tab == ResultsTab::Output {
            console_card(ui, "运行输出", &self.output_lines, true);
            return;
        }

        // ---- 结果列表页签 ----
        if self.results.is_empty() {
            card(ui, |ui| {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    if self.busy.is_some() {
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new("测速进行中").size(15.0).strong());
                        ui.label(
                            RichText::new("切到「运行输出」页签看实时日志").small().color(p.muted_fg),
                        );
                    } else {
                        ui.label(RichText::new("暂无结果").size(15.0).strong());
                        ui.label(
                            RichText::new("点上方「⚡ 一键测速并更新」或「仅测速」开始")
                                .small()
                                .color(p.muted_fg),
                        );
                    }
                });
                ui.add_space(8.0);
            });
            return;
        }

        // 绑定目标选择(与说明放在一起)
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("「写入Hosts」绑定:");
                let names: Vec<String> =
                    self.cfg.domains.iter().map(|d| d.name.clone()).collect();
                if names.is_empty() {
                    ui.label(RichText::new("(请先在「域名」页添加域名)").weak());
                } else {
                    let mut idx = self.bind_target.min(names.len() - 1);
                    egui::ComboBox::from_id_source("bind_target")
                        .selected_text(names[idx].clone())
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            for (i, n) in names.iter().enumerate() {
                                ui.selectable_value(&mut idx, i, n);
                            }
                        });
                    self.bind_target = idx;
                }
                ui.separator();
                let vip = self.cfg.cf_dns.domain.trim();
                if vip.is_empty() {
                    ui.label(RichText::new("「更新DNS」发布:(未配置,见设置)").small().weak());
                } else if let CfRecordView::Live(r) = &self.cf_record {
                    ui.label(
                        RichText::new(format!("「更新DNS」发布:{vip} → {}", r.ip))
                            .small()
                            .color(p.ok),
                    );
                } else {
                    ui.label(
                        RichText::new(format!("「更新DNS」发布:{vip}")).small().color(p.ok),
                    );
                }
            });
        });
        ui.add_space(8.0);

        // 结果表(克隆数据避免借用冲突)
        let results = self.results.clone();
        let pinned: Vec<Option<String>> =
            self.cfg.domains.iter().map(|d| d.pin_ip.clone()).collect();
        let target = self.bind_target;
        let dim_zero_speed = !self.cfg.speedtest.disable_download;

        card(ui, |ui| {
            table(ui, &[76.0, 64.0, 92.0, 56.0, 224.0])
            .header(28.0, |mut header| {
                header.col(|ui| table_head(ui, p, "IP"));
                header.col(|ui| table_head(ui, p, "延迟 ms"));
                header.col(|ui| table_head(ui, p, "丢包 %"));
                header.col(|ui| table_head(ui, p, "速度 MB/s"));
                header.col(|ui| table_head(ui, p, "地区"));
                header.col(|ui| table_head(ui, p, "操作"));
            })
            .body(|mut body| {
                for r in &results {
                    let is_pinned = pinned
                        .get(target)
                        .map(|pp| pp.as_deref() == Some(r.ip.as_str()))
                        .unwrap_or(false);
                    let dim = dim_zero_speed && r.speed_mb <= 0.0;
                    body.row(28.0, |mut row| {
                        row.col(|ui| {
                            if is_pinned {
                                ui.label(
                                    RichText::new(format!("★ {}", r.ip))
                                        .monospace()
                                        .strong()
                                        .color(p.ok),
                                );
                            } else if dim {
                                ui.label(RichText::new(&r.ip).monospace().weak());
                            } else {
                                ui.label(RichText::new(&r.ip).monospace());
                            }
                        });
                        row.col(|ui| {
                            let t = format!("{:.1}", r.avg_latency_ms);
                            ui.label(maybe_dim(&t, dim));
                        });
                        row.col(|ui| {
                            let t = format!("{:.2}", r.loss_rate);
                            ui.label(maybe_dim(&t, dim));
                        });
                        row.col(|ui| {
                            let t = format!("{:.2}", r.speed_mb);
                            if r.speed_mb > 0.0 {
                                ui.label(RichText::new(t).strong().color(p.ok));
                            } else {
                                ui.label(maybe_dim(&t, true));
                            }
                        });
                        row.col(|ui| {
                            let t = if r.region.is_empty() {
                                "—".to_string()
                            } else {
                                r.region.clone()
                            };
                            ui.label(maybe_dim(&t, dim));
                        });
                        row.col(|ui| {
                            if is_pinned {
                                if styled_button(
                                    ui,
                                    "取消选定",
                                    None,
                                    BtnStyle::OkSoft,
                                    BtnSize::Small,
                                    true,
                                    "取消选定,恢复自动跟随第一名并立即写入 hosts",
                                )
                                .clicked()
                                {
                                    acts.push(Act::ClearPin(target));
                                }
                            } else {
                                if styled_button(
                                    ui,
                                    "写入Hosts",
                                    None,
                                    BtnStyle::Ghost,
                                    BtnSize::Small,
                                    true,
                                    "把该 IP 绑定到上方选中的域名并立即写入 hosts",
                                )
                                .clicked()
                                {
                                    acts.push(Act::PinIp(target, r.ip.clone()));
                                }
                                ui.add_space(4.0);
                                if styled_button(
                                    ui,
                                    "更新DNS",
                                    None,
                                    BtnStyle::Ghost,
                                    BtnSize::Small,
                                    true,
                                    "绑定该 IP + 写入 hosts + 立即发布到 Cloudflare DNS(灰云 A 记录)",
                                )
                                .clicked()
                                {
                                    acts.push(Act::PinIpDns(target, r.ip.clone()));
                                }
                            }
                        });
                    });
                }
            });
        });
    }

    // ---------- UI:设置 ----------

    fn ui_settings(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let p = pal(ui);
        page_header(ui, "设置", "SETTINGS", "");

        // 设置项较多:卡片整体放进垂直滚动区,「保存设置」固定在底部
        egui::ScrollArea::vertical()
            .id_source("settings_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
        card(ui, |ui| {
            ui.label(RichText::new("测速引擎").strong());
            ui.add_space(4.0);
            egui::Grid::new("st_engine")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("cfst.exe 路径(空 = 自动探测):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ui_cfst_path).desired_width(380.0),
                    );
                    ui.end_row();

                    ui.label("cfst 探测结果:");
                    match &self.cfst_found {
                        Some(path) => {
                            ui.label(RichText::new(path).color(p.ok));
                        }
                        None => {
                            ui.label(
                                RichText::new("未找到(无法测速,请指定完整路径)").color(p.err),
                            );
                        }
                    };
                    ui.end_row();

                    ui.label("IP 段文件名(相对路径):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.speedtest.ip_file)
                            .desired_width(380.0),
                    );
                    ui.end_row();

                    ui.label("IP 段文件探测:");
                    match &self.ip_found {
                        Some(path) => {
                            ui.label(RichText::new(path).weak());
                        }
                        None => {
                            ui.label(RichText::new("未找到 ip.txt").color(p.err));
                        }
                    };
                    ui.end_row();
                });
        });
        ui.add_space(8.0);

        card(ui, |ui| {
            ui.label(RichText::new("测速参数").strong());
            ui.add_space(4.0);
            egui::Grid::new("st_params")
                .num_columns(4)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("平均延迟上限 -tl(ms,0 不限):");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.speedtest.max_latency_ms)
                            .clamp_range(0..=9999)
                            .suffix(" ms"),
                    );
                    ui.label("下载速度下限 -sl(MB/s,0 不限):");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.speedtest.min_speed_mb)
                            .clamp_range(0.0..=100.0)
                            .speed(0.1)
                            .suffix(" MB/s"),
                    );
                    ui.end_row();

                    ui.label("结果数量 -p:");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.speedtest.result_count)
                            .clamp_range(1..=100),
                    );
                    ui.label("单 IP 下载时长 -dt(秒):");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.speedtest.download_seconds)
                            .clamp_range(1..=60)
                            .suffix(" s"),
                    );
                    ui.end_row();

                    ui.label("只测延迟 -dd(跳过下载测速):");
                    ui.checkbox(&mut self.cfg.speedtest.disable_download, "");
                    ui.label("整体超时(秒):");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.speedtest.timeout_secs)
                            .clamp_range(60..=3600)
                            .suffix(" s"),
                    );
                    ui.end_row();
                });
        });
        ui.add_space(8.0);

        card(ui, |ui| {
            ui.label(RichText::new("Cloudflare DNS 动态发布").strong());
            ui.add_space(4.0);
            egui::Grid::new("cf_dns")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("启用动态发布:");
                    ui.checkbox(&mut self.cfg.cf_dns.enabled, "");
                    ui.end_row();

                    ui.label("API Token:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ui_cf_token)
                            .password(true)
                            .desired_width(380.0)
                            .hint_text("Cloudflare API Token(建议仅授 Zone.DNS Edit 权限)"),
                    );
                    ui.end_row();

                    ui.label("发布域名:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.cf_dns.domain)
                            .desired_width(380.0)
                            .hint_text("如 vip.example.com"),
                    );
                    ui.end_row();

                    ui.label("连接状态:");
                    ui.horizontal(|ui| {
                        if self.cfg.cf_dns.zone_id.is_some() {
                            ui.label(RichText::new("已验证(Zone 已缓存)").color(p.ok));
                        } else {
                            ui.label(RichText::new("未验证").weak());
                        }
                        if styled_button(
                            ui,
                            "验证连接",
                            None,
                            BtnStyle::Ghost,
                            BtnSize::Normal,
                            true,
                            "校验 Token 有效性并定位发布域名所在 Zone",
                        )
                        .clicked()
                        {
                            acts.push(Act::VerifyCfDns);
                        }
                        ui.add_space(6.0);
                        if styled_button(
                            ui,
                            "更新 DNS",
                            None,
                            BtnStyle::Ghost,
                            BtnSize::Normal,
                            true,
                            "立即把当前最优 IP 发布为发布域名的灰云 A 记录",
                        )
                        .clicked()
                        {
                            acts.push(Act::UpdateCfDns);
                        }
                    });
                    ui.end_row();

                    ui.label("当前记录:");
                    ui.horizontal(|ui| {
                        let can_query =
                            self.cfg.cf_dns.zone_id.is_some() && !self.cfg.cf_dns.domain.trim().is_empty();
                        match &self.cf_record {
                            CfRecordView::Unknown => {
                                ui.label(RichText::new("未查询").weak());
                            }
                            CfRecordView::Missing => {
                                ui.label(RichText::new("无 A 记录(尚未创建)").color(p.warn));
                            }
                            CfRecordView::Live(r) => {
                                // 橙云意味着 A 值不对外生效,提醒回灰云
                                let (cloud, color) =
                                    if r.proxied { ("橙云", p.warn) } else { ("灰云", p.ok) };
                                ui.label(
                                    RichText::new(r.ip.as_str()).monospace().strong().color(color),
                                );
                                ui.label(RichText::new(cloud).small().color(color));
                                if !r.updated.is_empty() {
                                    ui.label(
                                        RichText::new(format!("· {} 更新", r.updated))
                                            .small()
                                            .color(p.muted_fg),
                                    );
                                }
                            }
                            CfRecordView::Failed(e) => {
                                let msg: String = e.chars().take(30).collect();
                                ui.label(RichText::new(format!("查询失败:{msg}")).color(p.err));
                            }
                        }
                        ui.add_space(6.0);
                        if styled_button(
                            ui,
                            "查看记录",
                            None,
                            BtnStyle::Ghost,
                            BtnSize::Normal,
                            can_query,
                            "查询发布域名在 Cloudflare 上的当前 A 记录",
                        )
                        .clicked()
                        {
                            acts.push(Act::FetchCfRecord);
                        }
                    });
                    ui.end_row();
                });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "写入 hosts 成功后,自动把当前最优 IP 更新为发布域名的灰云 A 记录\
                     (TTL 60,强制不开 Cloudflare 代理);外部工具 dig 该域名即可获得当前优选 IP。\
                     Token 明文保存于本机配置文件。",
                )
                .small()
                .color(p.muted_fg),
            );
        });
        ui.add_space(8.0);

        card(ui, |ui| {
            ui.label(RichText::new("自动更新(内置定时器)").strong());
            ui.add_space(4.0);
            egui::Grid::new("sched")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("启用自动更新:");
                    ui.checkbox(&mut self.cfg.schedule.enabled, "");
                    ui.end_row();

                    ui.label("间隔(分钟):");
                    ui.add(
                        egui::DragValue::new(&mut self.cfg.schedule.interval_minutes)
                            .clamp_range(10..=10080)
                            .speed(5)
                            .suffix(" min"),
                    );
                    ui.end_row();

                    ui.label("上次更新:");
                    match self.cfg.last_run.at.as_deref().and_then(parse_local_dt) {
                        Some(t) => {
                            // 当天只显时刻,跨天补日期
                            let f = if t.date_naive() == chrono::Local::now().date_naive() {
                                "%H:%M"
                            } else {
                                "%m-%d %H:%M"
                            };
                            ui.label(RichText::new(t.format(f).to_string()).color(p.muted_fg));
                        }
                        None => {
                            ui.label(RichText::new("—").weak());
                        }
                    }
                    ui.end_row();

                    ui.label("下次自动更新:");
                    if self.cfg.schedule.enabled {
                        match self.last_auto_at {
                            Some(t) => {
                                let next = t
                                    + chrono::Duration::minutes(
                                        self.cfg.schedule.interval_minutes.max(1) as i64,
                                    );
                                // 间隔可设到 7 天:跨天时只显 HH:MM 会误导读
                                let f = if next.date_naive() == chrono::Local::now().date_naive() {
                                    "%H:%M"
                                } else {
                                    "%m-%d %H:%M"
                                };
                                ui.label(RichText::new(next.format(f).to_string()).color(p.ok));
                            }
                            None => {
                                ui.label(RichText::new("即将执行").color(p.ok));
                            }
                        }
                    } else {
                        ui.label(RichText::new("未启用").weak());
                    }
                    ui.end_row();
                });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "到点自动执行:测速 → 写入 hosts → 更新 Cloudflare DNS(与「一键测速并更新」\
                     相同链路)。定时器内置于程序,跨平台、无需注册系统计划任务,但需保持本程序运行;\
                     到点时若正忙则顺延至空闲。写入 hosts 需要权限,未以管理员运行时会弹 UAC。",
                )
                .small()
                .color(p.muted_fg),
            );
        });
            });
        ui.add_space(10.0);
        if styled_button(ui, "保存设置", None, BtnStyle::Primary, BtnSize::Normal, true, "")
            .clicked()
        {
            acts.push(Act::SaveConfig);
        }
    }

    // ---------- UI:日志 ----------

    fn ui_logs(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        page_header(ui, "日志", "LOGS", "仅显示本次运行;完整日志在 %APPDATA%\\cf-hosts-gui\\logs\\");
        let entries = self.log.snapshot();
        let lines: Vec<String> = entries
            .iter()
            .map(|e| format!("[{}] {} {}", e.time, e.level, e.msg))
            .collect();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("运行日志(最新在上)").strong());
                if styled_button(ui, "复制", None, BtnStyle::Ghost, BtnSize::Small, true, "复制全部日志到剪贴板")
                    .clicked()
                {
                    ui.ctx().output_mut(|o| o.copied_text = lines.join("\n"));
                }
            });
            ui.add_space(2.0);
            // 终端风内衬
            egui::Frame::default()
                .fill(p.deep)
                .rounding(egui::Rounding::same(6.0))
                .inner_margin(egui::Margin::same(10.0))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height((ui.available_height() - 16.0).max(80.0))
                        .show(ui, |ui| {
                            for e in entries.iter().rev() {
                                let color = match e.level {
                                    "ERROR" => p.err,
                                    "WARN" => p.warn,
                                    _ => p.muted_fg,
                                };
                                ui.label(
                                    RichText::new(format!("{} [{}] {}", e.time, e.level, e.msg))
                                        .color(color)
                                        .monospace()
                                        .small(),
                                );
                            }
                        });
                });
        });
    }
}

impl eframe::App for CfApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui_update(ctx);
    }
}

// ---------- 主题(设计 token → egui visuals) ----------

fn apply_visuals(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    ctx.set_visuals(if dark { egui::Visuals::dark() } else { egui::Visuals::light() });
    ctx.style_mut(|s| {
        // 分层背景:窗口底 → 面板 → 卡片(glassmorphism 的层深,无模糊用色阶表达)
        s.visuals.dark_mode = dark;
        s.visuals.panel_fill = p.panel;
        s.visuals.window_fill = p.panel;
        s.visuals.extreme_bg_color = p.deep;
        s.visuals.faint_bg_color = p.card_alt;

        let r8 = egui::Rounding::same(8.0);
        s.visuals.widgets.inactive.rounding = r8;
        s.visuals.widgets.hovered.rounding = r8;
        s.visuals.widgets.active.rounding = r8;
        s.visuals.widgets.open.rounding = r8;
        s.visuals.window_rounding = egui::Rounding::same(10.0);
        s.visuals.menu_rounding = r8;

        // 次级按钮:卡片色底 + 细边;悬停提亮
        s.visuals.widgets.inactive.bg_fill = mix(p.card, p.fg, if dark { 0.06 } else { 0.03 });
        s.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, p.border_strong);
        s.visuals.widgets.hovered.bg_fill = mix(p.card, p.fg, if dark { 0.12 } else { 0.06 });
        s.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, p.border_strong);
        s.visuals.widgets.active.bg_fill = mix(p.card, p.fg, if dark { 0.16 } else { 0.10 });
        // 输入框/下拉底色
        s.visuals.widgets.inactive.weak_bg_fill = mix(p.deep, p.card, 0.7);

        // 选中态(导航项/下拉/文本选区):accent 实底 + on_accent 前景。
        // (此前 ok 绿字配暗绿底,同色系难辨,选中的字看不清)
        s.visuals.selection.bg_fill = p.accent;
        s.visuals.selection.stroke = egui::Stroke::new(1.0_f32, p.on_accent);
        s.visuals.hyperlink_color = p.ok;

        // 字号阶梯(13 / 14 / 15 / 20;整数避免半像素模糊,小字上调提升可读性)
        use egui::{FontId, TextStyle};
        s.text_styles = [
            (TextStyle::Heading, FontId::proportional(20.0)),
            (TextStyle::Body, FontId::proportional(15.0)),
            (TextStyle::Monospace, FontId::monospace(14.0)),
            (TextStyle::Button, FontId::proportional(15.0)),
            (TextStyle::Small, FontId::proportional(13.0)),
        ]
        .into();

        // 8px 间距节奏
        s.spacing.item_spacing = egui::vec2(10.0, 6.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 26.0;
        // 细滚动条
        s.spacing.scroll.bar_width = 10.0;
        s.spacing.scroll.bar_inner_margin = 2.0;
        s.spacing.scroll.bar_outer_margin = 0.0;
    });
}

// ---------- 组件小工具 ----------

/// 分组卡片:card 色底 + 1px 边 + 10px 圆角 + 14px 内距
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let p = pal(ui);
    egui::Frame::default()
        .fill(p.card)
        .stroke(egui::Stroke::new(1.0_f32, p.border))
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::same(14.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui);
        });
}

/// 状态胶囊:token 色 12% 底 + token 色文字


fn page_header(ui: &mut egui::Ui, title: &str, tag: &str, sub: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(18.0).strong());
        ui.label(RichText::new(tag).small().color(p.muted_fg).monospace());
        if !sub.is_empty() {
            ui.label(RichText::new(format!("· {sub}")).small().color(p.muted_fg));
        }
    });
    ui.add_space(6.0);
}

/// 表格构建器:第一列 remainder,其余固定宽;调用方继续链 .header().body()
fn table<'a>(ui: &'a mut egui::Ui, widths: &[f32]) -> TableBuilder<'a> {
    let mut tb = TableBuilder::new(ui)
        .striped(true)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder());
    for w in widths {
        tb = tb.column(Column::initial(*w));
    }
    tb
}

fn table_head(ui: &mut egui::Ui, p: Palette, text: &str) {
    ui.label(RichText::new(text).small().strong().color(p.muted_fg));
}

/// 控制台卡片:卡片内再叠一层 deep 色"终端"底
fn console_card(ui: &mut egui::Ui, title: &str, lines: &[String], fill: bool) {
    let p = pal(ui);
    let max_h = if fill {
        (ui.available_height() - 60.0).max(100.0)
    } else {
        170.0
    };
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).strong());
            if styled_button(ui, "复制", None, BtnStyle::Ghost, BtnSize::Small, true, "复制全部输出到剪贴板")
                .clicked()
            {
                ui.ctx().output_mut(|o| o.copied_text = lines.join("\n"));
            }
        });
        ui.add_space(2.0);
        egui::Frame::default()
            .fill(p.deep)
            .rounding(egui::Rounding::same(6.0))
            .inner_margin(egui::Margin::same(10.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .max_height(max_h)
                    .show(ui, |ui| {
                        if lines.is_empty() {
                            ui.label(RichText::new("(暂无输出)").weak().monospace().small());
                        }
                        for l in lines {
                            ui.label(RichText::new(l).monospace().small());
                        }
                    });
            });
    });
}

// ---------- 内嵌页签(下划线样式:无框无底,与按钮明确区分) ----------

/// 下划线页签:active = 加粗文字 + 底部 accent 短下划线(压在页签条基线上)
fn seg_tab(ui: &mut egui::Ui, label: &str, active: bool) -> bool {
    let p = pal(ui);
        let font = egui::FontId::proportional(if active { 15.0 } else { 14.0 });
    // 两遍排版:先量宽,再烘焙最终颜色(galley 颜色在 layout 时固定)
    let text_w = ui
        .painter()
        .layout(label.to_owned(), font.clone(), Color32::WHITE, f32::INFINITY)
        .size()
        .x;
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(text_w + 20.0, 30.0), egui::Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    let color = if active || hovered { p.fg } else { p.muted_fg };
    let galley = ui.painter().layout(label.to_owned(), font, color, f32::INFINITY);
    let pos = egui::pos2(rect.center().x - galley.size().x * 0.5, rect.top() + 5.0);
    ui.painter().galley(pos, galley, color);
    if active {
        // 底部 accent 下划线,略窄于页签,压在下方基线上
        let bar = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 3.0, rect.bottom() - 3.0),
            egui::pos2(rect.right() - 3.0, rect.bottom() - 0.5),
        );
        ui.painter().rect_filled(bar, egui::Rounding::same(2.0), p.accent);
    }
    resp.clicked()
}

// ---------- 自绘按钮(全软件统一:Primary/Ghost/Danger/OkSoft × Normal/Small) ----------

enum BtnStyle {
    /// 主操作:accent 实底 + on_accent 深字
    Primary,
    /// 次操作:透明底 + 细描边,悬停轻提亮
    Ghost,
    /// 破坏性:平时同幽灵,悬停转警示红
    Danger,
    /// 已激活状态(如"取消选定"):ok 色浅底
    OkSoft,
}

enum BtnSize {
    /// 动作条/设置页:32px 高,14.5 字号
    Normal,
    /// 表格行内/卡片头部:24px 高,12.5 字号
    Small,
}

/// 自绘按钮:悬停反馈 + 手型光标;Normal 8px 圆角 / Small 6px
fn styled_button(
    ui: &mut egui::Ui,
    label: &str,
    icon: Option<NavIcon>,
    style: BtnStyle,
    size: BtnSize,
    enabled: bool,
    tip: &str,
) -> egui::Response {
    let p = pal(ui);
    let (font_size, height, pad_x, icon_w, rounding) = match size {
    BtnSize::Normal => (15.0, 32.0, 14.0, 22.0, 8.0),
    BtnSize::Small => (13.0, 24.0, 10.0, 18.0, 6.0),
    };
    let font = egui::FontId::proportional(font_size);
    // 第一遍排版仅测宽(悬停态决定颜色,颜色又需在排版时烘焙,故两遍)
    let text_w = ui
        .painter()
        .layout(label.to_owned(), font.clone(), Color32::WHITE, f32::INFINITY)
        .size()
        .x;
    let has_icon = icon.is_some();
    let btn_size = egui::vec2(
        pad_x + if has_icon { icon_w } else { 0.0 } + text_w + pad_x,
        height,
    );
    let (rect, resp) = ui.allocate_exact_size(btn_size, egui::Sense::click());
    let hovered = enabled && resp.hovered();
    if hovered {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }

    let (fill, stroke, text_color, icon_color) = match style {
        BtnStyle::Primary => {
            if enabled {
                let fill = if hovered { mix(p.accent, p.fg, 0.12) } else { p.accent };
                (fill, egui::Stroke::NONE, p.on_accent, p.on_accent)
            } else {
                (mix(p.accent, p.panel, 0.55), egui::Stroke::NONE, p.muted_fg, p.muted_fg)
            }
        }
        BtnStyle::Ghost => {
            if enabled {
                let fill = if hovered { mix(p.card, p.fg, 0.08) } else { Color32::TRANSPARENT };
                (
                    fill,
                    egui::Stroke::new(1.0_f32, p.border_strong),
                    p.fg,
                    p.muted_fg,
                )
            } else {
                (
                    Color32::TRANSPARENT,
                    egui::Stroke::new(1.0_f32, p.border),
                    p.muted_fg,
                    p.muted_fg,
                )
            }
        }
        BtnStyle::Danger => {
            if enabled {
                if hovered {
                    (
                        mix(p.card, p.err, 0.16),
                        egui::Stroke::new(1.0_f32, mix(p.border_strong, p.err, 0.6)),
                        p.err,
                        p.err,
                    )
                } else {
                    (
                        Color32::TRANSPARENT,
                        egui::Stroke::new(1.0_f32, p.border_strong),
                        p.muted_fg,
                        p.muted_fg,
                    )
                }
            } else {
                (
                    Color32::TRANSPARENT,
                    egui::Stroke::new(1.0_f32, p.border),
                    p.muted_fg,
                    p.muted_fg,
                )
            }
        }
        BtnStyle::OkSoft => {
            if hovered {
                (
                    mix(p.card, p.ok, 0.24),
                    egui::Stroke::new(1.0_f32, p.ok),
                    p.ok,
                    p.ok,
                )
            } else {
                (
                    mix(p.card, p.ok, 0.16),
                    egui::Stroke::new(1.0_f32, mix(p.border, p.ok, 0.5)),
                    p.ok,
                    p.ok,
                )
            }
        }
    };

    ui.painter().rect(rect, egui::Rounding::same(rounding), fill, stroke);
    if let Some(kind) = icon {
        draw_nav_icon(ui, kind, rect.left_center() + egui::vec2(pad_x * 0.8, 0.0), icon_color);
    }
    // 第二遍:把最终文字颜色烘焙进 galley(galley() 的兜底色参数不会覆盖已烘焙色)
    let galley = ui
        .painter()
        .layout(label.to_owned(), font, text_color, f32::INFINITY);
    let text_x = rect.left() + pad_x + if has_icon { icon_w } else { 0.0 };
    let text_pos = egui::pos2(text_x, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(text_pos, galley, text_color);

    if !tip.is_empty() {
        if enabled {
            return resp.on_hover_text(tip);
        }
        return resp.on_disabled_hover_text(tip);
    }
    resp
}

// ---------- 矢量图标(单线同族:16px 画布 / 1.6px 描边) ----------

enum NavIcon {
    Link,
    Bolt,
    Gear,
    Lines,
    Gauge,
}

/// 参数化胶囊(链条一节):给定方向/半长/半径的圆角长条轮廓点集
fn capsule_outline(center: egui::Pos2, dir: egui::Vec2, half: f32, r: f32) -> Vec<egui::Pos2> {
    let mut pts = Vec::new();
    let a = center - dir * half;
    let b = center + dir * half;
    let perp = egui::vec2(-dir.y, dir.x);
    let steps = 8;
    for i in 0..=steps {
        let t = -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / steps as f32;
        pts.push(b + dir * (r * t.cos()) + perp * (r * t.sin()));
    }
    for i in 0..=steps {
        let t = std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / steps as f32;
        pts.push(a + dir * (r * t.cos()) + perp * (r * t.sin()));
    }
    pts
}

fn draw_nav_icon(ui: &egui::Ui, kind: NavIcon, center: egui::Pos2, color: Color32) {
    let s = egui::Stroke::new(1.6_f32, color);
    let pt = ui.painter();
    match kind {
        NavIcon::Link => {
            let d = egui::vec2(0.7071, 0.7071); // 45°
            for c in [center - d * 3.2, center + d * 3.2] {
                let points = capsule_outline(c, d, 2.8, 2.0);
                pt.add(egui::Shape::Path(egui::epaint::PathShape {
                    points,
                    closed: true,
                    fill: egui::Color32::TRANSPARENT,
                    stroke: s,
                }));
            }
        }
        NavIcon::Bolt => draw_bolt_at(pt, center, 7.0, s),
        NavIcon::Gauge => {
            // 仪表盘:上半圆表盘(开口向下)+ 右上指针 + 中心点
            let r = 7.0;
            let mut pts: Vec<egui::Pos2> = Vec::new();
            for i in 0..=12 {
                let t = std::f32::consts::PI * i as f32 / 12.0; // 0..π,左→右
                pts.push(center + egui::vec2(-r * t.cos(), -r * t.sin()));
            }
            pt.add(egui::Shape::Path(egui::epaint::PathShape {
                points: pts,
                closed: false,
                fill: egui::Color32::TRANSPARENT,
                stroke: s,
            }));
            let ang: f32 = 0.82; // 指针角(数学角,右上方向)
            let tip = center + egui::vec2(5.0 * ang.cos(), -5.0 * ang.sin());
            pt.line_segment([center, tip], s);
            pt.circle_filled(center, 1.4, color);
        }
        NavIcon::Gear => {
            pt.circle_stroke(center, 3.5, s);
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let dir = egui::vec2(a.cos(), a.sin());
                pt.line_segment([center + dir * 5.5, center + dir * 7.5], s);
            }
        }
        NavIcon::Lines => {
            for dy in [-5.0, 0.0, 5.0] {
                pt.line_segment(
                    [center + egui::vec2(-6.0, dy), center + egui::vec2(6.0, dy)],
                    s,
                );
            }
        }
    }
}

/// 云朵(品牌图形,Cloudflare 隐喻):三段圆弧 + 底弦,单线描边
fn draw_cloud(ui: &mut egui::Ui, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let c = rect.center();
    let s = size / 20.0; // 设计基准 20px 画布
    // (中心偏移, 半径, 起角, 终角) — 数学角,绘制时 y 取负(屏幕 y 向下)
    let arcs: [((f32, f32), f32, f32, f32); 3] = [
        ((-4.6, 0.6), 2.8, -90.0, -270.0), // 左鼓包:底→左→顶
        ((0.2, -0.8), 4.2, 160.0, 20.0),   // 中大鼓包:左下→顶→右下
        ((4.6, 0.6), 2.9, 90.0, -60.0),    // 右鼓包:顶→右→底
    ];
    let mut pts: Vec<egui::Pos2> = Vec::new();
    for ((cx, cy), r, a0, a1) in arcs {
        let n = 10;
        for i in 0..=n {
            let t = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
            pts.push(c + egui::vec2(cx * s + r * s * t.cos(), cy * s - r * s * t.sin()));
        }
    }
    // closepath 底弦连接右下→左下,即云的平底
    ui.painter().add(egui::Shape::Path(egui::epaint::PathShape {
        points: pts,
        closed: true,
        fill: egui::Color32::TRANSPARENT,
        stroke: egui::Stroke::new(1.8_f32, color),
    }));
}

fn draw_bolt_at(pt: &egui::Painter, center: egui::Pos2, r: f32, stroke: egui::Stroke) {
    let pts_rel: [egui::Vec2; 7] = [
        egui::vec2(0.62 * r, -r),
        egui::vec2(-0.55 * r, 0.18 * r),
        egui::vec2(0.02 * r, 0.18 * r),
        egui::vec2(-0.62 * r, r),
        egui::vec2(0.55 * r, -0.18 * r),
        egui::vec2(-0.02 * r, -0.18 * r),
        egui::vec2(0.25 * r, -r),
    ];
    let points: Vec<egui::Pos2> = pts_rel.iter().map(|v| center + *v).collect();
    let shape = egui::epaint::PathShape {
        points,
        closed: true,
        fill: egui::Color32::TRANSPARENT,
        stroke,
    };
    pt.add(egui::Shape::Path(shape));
}

fn maybe_dim(t: &str, dim: bool) -> RichText {
    if dim {
        RichText::new(t).weak()
    } else {
        RichText::new(t)
    }
}

fn fmt_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

// ---------- 窗口图标(绿色圆角方块 + 白色闪电,与品牌图形一致) ----------

fn window_icon() -> Option<egui::IconData> {
    let rgba = icon_rgba_64();
    Some(egui::IconData { rgba, width: 64, height: 64 })
}

/// 64x64 程序图标 RGBA(窗口图标与托盘图标共用同一绘制)
pub fn icon_rgba_64() -> Vec<u8> {
    let s = 64i32;
    let mut rgba = vec![0u8; (s * s * 4) as usize];
    let in_round_rect = |x: f32, y: f32| -> bool {
        let (r, m) = (14.0, 3.0);
        let max = s as f32 - m;
        if x < m || x > max || y < m || y > max {
            return false;
        }
        let (cx, cy) = ((x - m).min(max - x), (y - m).min(max - y));
        if cx > r || cy > r {
            return true;
        }
        (cx - r).hypot(cy - r) <= r
    };
    let bolt: [(f32, f32); 7] = [
        (41.0, 6.0),
        (20.0, 37.0),
        (30.0, 37.0),
        (23.0, 58.0),
        (46.0, 25.0),
        (34.0, 25.0),
        (44.0, 6.0),
    ];
    let in_poly = |px: f32, py: f32| -> bool {
        let mut inside = false;
        let mut j = bolt.len() - 1;
        for i in 0..bolt.len() {
            let (xi, yi) = bolt[i];
            let (xj, yj) = bolt[j];
            if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
                inside = !inside;
            }
            j = i;
        }
        inside
    };
    for y in 0..s {
        for x in 0..s {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (r, g, b, a) = if in_round_rect(fx, fy) {
                if in_poly(fx, fy) {
                    (255, 255, 255, 255)
                } else {
                    (0x22, 0xC5, 0x5E, 255)
                }
            } else {
                (0, 0, 0, 0)
            };
            let i = ((y * s + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = a;
        }
    }
    rgba
}

// ---------- CJK 字体(egui 默认字体无中文;优先用体积小的黑体省内存) ----------

fn setup_fonts(ctx: &egui::Context) {
    // Windows:内嵌子集字体(按 src/ 源码全部字符生成;新增 UI 文案后重跑
    // tools/make_font_subset.py 再编译,否则新字符会显示为方框)
    #[cfg(windows)]
    let font_bytes: Option<Vec<u8>> =
        Some(include_bytes!("../assets/cjk-subset.ttf").to_vec());
    // 其他平台:探测系统字体
    #[cfg(not(windows))]
    let font_bytes: Option<Vec<u8>> = {
        let candidates: &[&str] = if cfg!(target_os = "macos") {
            &[
                "/System/Library/Fonts/PingFang.ttc",
                "/System/Library/Fonts/Hiragino Sans GB.ttc",
            ]
        } else {
            &[
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
            ]
        };
        candidates.iter().find_map(|p| std::fs::read(p).ok())
    };

    if let Some(bytes) = font_bytes {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert("cjk".into(), egui::FontData::from_owned(bytes));
        // 回退字体:雅黑无 ✓✗⚠⚡ 等符号(会渲染成方框),用 Segoe UI Symbol 子集补
        #[cfg(windows)]
        {
            fonts.font_data.insert(
                "syms".into(),
                egui::FontData::from_owned(include_bytes!("../assets/symbols-subset.ttf").to_vec()),
            );
        }
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.push("cjk".into());
                // families 内按序回退:cjk 缺字时落到 syms
                #[cfg(windows)]
                list.push("syms".into());
            }
        }
        ctx.set_fonts(fonts);
        return;
    }
    eprintln!("警告:未找到中文字体,界面中文可能显示为方块");
}

// ---------- 空闲内存优化:修剪工作集(Windows) ----------

#[cfg(windows)]
fn trim_working_set() {
    use windows_sys::Win32::System::Threading::SetProcessWorkingSetSize;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // (SIZE_MAX, SIZE_MAX) = 把当前工作集页面换出,交互时按需换回
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX); }
}
#[cfg(not(windows))]
fn trim_working_set() {}

pub fn run_gui() {
    let dark = Config::load().dark_mode;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([930.0, 700.0])
            .with_title("CF Hosts GUI")
            .with_icon(window_icon().unwrap()),
        ..Default::default()
    };
    eframe::run_native(
        "cf-hosts-gui",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            apply_visuals(&cc.egui_ctx, dark);
            #[cfg(windows)]
            if let Err(e) = crate::tray::init(&cc.egui_ctx) {
                eprintln!("托盘初始化失败:{e}");
            }
            Box::new(CfApp::new())
        }),
    )
    .expect("egui 运行失败");
}
