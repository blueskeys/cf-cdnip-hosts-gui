//! `--selftest`:不触碰系统 hosts 的核心逻辑自检(hosts 计划构建 / CSV 解析 / 幂等性)。
//! 同一套检查同时以 `cargo test` 单元测试形式运行。

use crate::config::app_dir;
use crate::hosts::{build_plan, effective_ip, HostsEntry, WriteJob, WriteMode, MARK_BEGIN, MARK_END};
use crate::speedtest::parse_csv;

fn job_replace(entries: Vec<(&str, &str)>, meta: &str) -> WriteJob {
    WriteJob {
        mode: WriteMode::Replace,
        entries: entries
            .into_iter()
            .map(|(ip, domain)| HostsEntry { ip: ip.into(), domain: domain.into() })
            .collect(),
        meta: meta.into(),
    }
}

/// 返回 (名称, 是否通过) 列表
pub fn checks() -> Vec<(&'static str, bool)> {
    let mut v: Vec<(&'static str, bool)> = Vec::new();

    // --- 1. CSV 解析(UTF-8 BOM + 7 列 + -dd 5 列格式) ---
    let sample = "\u{feff}IP 地址,已发送,已接收,丢包率,平均延迟,下载速度(MB/s),地区码\n\
                  172.67.69.143,4,4,0.00,91.63,23.23,HKG\n\
                  104.26.15.102,4,4,0.00,66.65,39.41,HKG\n";
    let csv_path = app_dir().join("selftest-sample.csv");
    let _ = std::fs::create_dir_all(app_dir());
    let ok = std::fs::write(&csv_path, sample.as_bytes())
        .ok()
        .and_then(|_| parse_csv(&csv_path).ok())
        .map(|r| {
            r.len() == 2
                && r[0].ip == "172.67.69.143"
                && (r[0].avg_latency_ms - 91.63).abs() < 0.01
                && (r[0].speed_mb - 23.23).abs() < 0.01
                && r[0].region == "HKG"
                && r[1].ip == "104.26.15.102"
        })
        .unwrap_or(false);
    v.push(("CSV 解析(7 列 + BOM)", ok));

    // --- 2. 首次写入:无区块 → 追加;区块外冲突行被注释 ---
    let cur1 = "127.0.0.1 localhost\n10.0.0.1 other.example\n104.26.15.102 cc.x.top\n";
    let plan = build_plan(cur1, &job_replace(vec![("1.2.3.4", "cc.x.top")], "selftest"));
    let lines: Vec<&str> = plan.new_content.lines().collect();
    v.push((
        "首次写入生成区块",
        plan.changed
            && plan.new_content.contains(MARK_BEGIN)
            && plan.new_content.contains(MARK_END)
            && plan.new_content.contains("1.2.3.4 cc.x.top"),
    ));
    v.push((
        "区块外冲突行被注释",
        lines.iter().any(|l| l.trim().starts_with("# 104.26.15.102 cc.x.top"))
            && !lines.iter().any(|l| l.trim() == "104.26.15.102 cc.x.top")
            && lines.iter().any(|l| l.trim() == "10.0.0.1 other.example"), // 无关行不动
    ));

    // --- 3. 已有区块 → 原地替换 IP ---
    let cur2 = "127.0.0.1 localhost\n\
                # --- cf-hosts-gui BEGIN ---\n\
                # updated: 2026-10-31 21:00 (best: 9.9.9.9 90ms 20.0MB/s HKG)\n\
                9.9.9.9 cc.x.top\n\
                # --- cf-hosts-gui END ---\n";
    let plan2 = build_plan(cur2, &job_replace(vec![("1.2.3.4", "cc.x.top")], "selftest"));
    let l2: Vec<&str> = plan2.new_content.lines().collect();
    v.push((
        "已有区块原地替换",
        plan2.changed
            && l2.iter().any(|l| l.trim() == "1.2.3.4 cc.x.top")
            && !l2.iter().any(|l| l.trim() == "9.9.9.9 cc.x.top")
            && l2.iter().any(|l| l.trim() == "127.0.0.1 localhost")
            && plan2.new_content.matches(MARK_BEGIN).count() == 1, // 不会出现双区块
    ));

    // --- 4. 幂等:相同内容再次写入 → changed == false ---
    let plan3 = build_plan(&plan2.new_content, &job_replace(vec![("1.2.3.4", "cc.x.top")], "selftest"));
    v.push(("幂等写入(无变更)", !plan3.changed));

    // --- 5. 还原:RemoveAll 移除区块,无关内容保留 ---
    let plan4 = build_plan(
        &plan2.new_content,
        &WriteJob { mode: WriteMode::RemoveAll, entries: vec![], meta: String::new() },
    );
    v.push((
        "RemoveAll 还原",
        plan4.changed
            && !plan4.new_content.contains(MARK_BEGIN)
            && !plan4.new_content.contains(MARK_END)
            && !plan4.new_content.contains("cc.x.top")
            && plan4.new_content.contains("127.0.0.1 localhost"),
    ));

    // --- 6. 无区块时 RemoveAll → 无变更 ---
    let plan5 = build_plan(
        "127.0.0.1 localhost\n",
        &WriteJob { mode: WriteMode::RemoveAll, entries: vec![], meta: String::new() },
    );
    v.push(("无区块时还原为空操作", !plan5.changed));

    // --- 7. effective_ip:首条命中 ---
    v.push((
        "域名生效 IP 查询(首条命中)",
        effective_ip("1.1.1.1 d.top\n2.2.2.2 d.top\n", "D.TOP").as_deref() == Some("1.1.1.1")
            && effective_ip("1.1.1.1 d.top\n", "e.top").is_none(),
    ));

    v
}

/// `--selftest` 入口:打印报告 + 写报告文件(release 版为 GUI 子系统无控制台)
pub fn run() {
    let results = checks();
    let mut report = String::from("cf-hosts-gui selftest\n====================\n");
    let mut fail = 0;
    for (name, ok) in &results {
        report.push_str(&format!("[{}] {}\n", if *ok { "PASS" } else { "FAIL" }, name));
        if !ok {
            fail += 1;
        }
    }
    report.push_str(&format!(
        "\n总计: {} 通过 / {} 失败\n",
        results.len() - fail,
        fail
    ));
    print!("{report}");

    let path = app_dir().join("selftest-report.txt");
    let _ = std::fs::create_dir_all(app_dir());
    let _ = std::fs::write(&path, &report);
    println!("报告已写入: {}", path.display());

    std::process::exit(if fail == 0 { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    #[test]
    fn selftest_checks_all_pass() {
        let results = super::checks();
        let fails: Vec<&str> = results
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(name, _)| *name)
            .collect();
        assert!(fails.is_empty(), "自检未通过: {fails:?}");
    }
}
