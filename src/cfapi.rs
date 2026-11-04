//! Cloudflare API v4 客户端:把当前优选 IP 动态发布到一条灰云 A 记录。
//! 纯 HTTPS 出站调用,无需管理员权限;发布域名与 Token 均来自用户配置,
//! 本模块不含任何真实域名。

use crate::hosts::WriteOutcome;
use serde_json::{json, Value};

const API_BASE: &str = "https://api.cloudflare.com/client/v4";
/// A 记录 TTL(秒):足够短,外部查询能较快跟随优选结果切换
const RECORD_TTL: u64 = 60;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

/// 解析 Cloudflare 统一响应体:success=false 时拼接 errors[].message
fn parse_cf(v: Value) -> Result<Value, String> {
    if v["success"].as_bool() == Some(true) {
        Ok(v["result"].clone())
    } else {
        let errs: Vec<String> = v["errors"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e["message"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Err(if errs.is_empty() { "Cloudflare 返回未知错误".into() } else { errs.join("; ") })
    }
}

fn get(token: &str, path: &str) -> Result<Value, String> {
    let resp = agent()
        .get(&format!("{API_BASE}{path}"))
        .set("Authorization", &format!("Bearer {token}"))
        .call();
    match resp {
        Ok(r) => parse_cf(r.into_json::<Value>().map_err(|e| format!("响应解析失败: {e}"))?),
        Err(ureq::Error::Status(_, r)) => {
            parse_cf(r.into_json::<Value>().unwrap_or(Value::Null))
        }
        Err(e) => Err(format!("网络错误: {e}")),
    }
}

fn send_json(token: &str, method: &str, path: &str, body: Value) -> Result<Value, String> {
    let resp = agent()
        .request(method, &format!("{API_BASE}{path}"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body);
    match resp {
        Ok(r) => parse_cf(r.into_json::<Value>().map_err(|e| format!("响应解析失败: {e}"))?),
        Err(ureq::Error::Status(_, r)) => {
            parse_cf(r.into_json::<Value>().unwrap_or(Value::Null))
        }
        Err(e) => Err(format!("网络错误: {e}")),
    }
}

/// 校验 Token 有效性,成功返回状态描述
pub fn verify(token: &str) -> Result<String, String> {
    let r = get(token, "/user/tokens/verify")?;
    let status = r["status"].as_str().unwrap_or("unknown");
    if status == "active" {
        Ok("Token 有效".into())
    } else {
        Err(format!("Token 状态异常: {status}"))
    }
}

/// zone 候选序列:从完整域名逐级去掉最左侧标签,直到根域(两级)。
/// 例:a.b.example.com → b.example.com → example.com
pub fn zone_candidates(domain: &str) -> Vec<String> {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = d.split('.').filter(|s| !s.is_empty()).collect();
    (2..=labels.len())
        .rev()
        .map(|take| labels[labels.len() - take..].join("."))
        .collect()
}

/// 由发布域名逐级定位 zone(如 a.b.example.com → b.example.com → example.com),
/// 返回 zone_id。zone 必须在该 Token 可访问的账号内。
pub fn resolve_zone(token: &str, domain: &str) -> Result<String, String> {
    if domain.trim().trim_end_matches('.').is_empty() {
        return Err("发布域名为空".into());
    }
    let label_count = domain
        .trim()
        .trim_end_matches('.')
        .split('.')
        .filter(|s| !s.is_empty())
        .count();
    if label_count < 2 {
        return Err("发布域名格式无效".into());
    }
    for candidate in zone_candidates(domain) {
        let r = get(token, &format!("/zones?name={candidate}&status=active"))?;
        if let Some(id) = r
            .as_array()
            .and_then(|a| a.first())
            .and_then(|z| z["id"].as_str())
            .map(String::from)
        {
            return Ok(id);
        }
    }
    let d = domain.trim().trim_end_matches('.');
    Err(format!(
        "未找到 {d} 的 Zone:Token 缺少「Zone → Zone → Read」权限,或 Zone Resources 未含该域名"
    ))
}

/// 把 ip 写到 domain 的 A 记录(不存在则创建)。
/// 强制灰云(proxied=false):若已存在橙云记录,更新时一并改为灰云。
pub fn upsert_record(token: &str, zone_id: &str, domain: &str, ip: &str) -> WriteOutcome {
    let inner = || -> Result<String, String> {
        let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        let list =
            get(token, &format!("/zones/{zone_id}/dns_records?type=A&name={d}"))?;
        let existing = list.as_array().and_then(|a| a.first()).cloned();
        let body = json!({
            "type": "A",
            "name": d,
            "content": ip,
            "ttl": RECORD_TTL,
            // 强制灰云:开橙云时公网解析由 CF 调度,A 值不会对外生效,优选即失效
            "proxied": false,
        });
        match existing {
            Some(rec) => {
                let id = rec["id"].as_str().ok_or("记录 id 缺失")?;
                let old = rec["content"].as_str().unwrap_or("?").to_string();
                let was_proxied = rec["proxied"].as_bool().unwrap_or(false);
                if old == ip && !was_proxied {
                    return Ok(format!("{d} 的 A 记录已是 {ip}(灰云),无需更新"));
                }
                send_json(token, "PUT", &format!("/zones/{zone_id}/dns_records/{id}"), body)?;
                let mut msg = format!("{d}: {old} → {ip}");
                if was_proxied {
                    msg.push_str("(已从橙云改为灰云)");
                }
                Ok(msg)
            }
            None => {
                send_json(token, "POST", &format!("/zones/{zone_id}/dns_records"), body)?;
                Ok(format!("已为 {d} 创建灰云 A 记录 → {ip}"))
            }
        }
    };
    match inner() {
        Ok(msg) => WriteOutcome {
            ok: true,
            message: format!("Cloudflare DNS 发布成功:{msg}"),
            diff: vec![msg],
        },
        Err(e) => WriteOutcome::fail(format!("Cloudflare DNS 发布失败:{e}")),
    }
}
