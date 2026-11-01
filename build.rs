// 把 assets/icon.ico 嵌入 exe 的 PE 资源(任务管理器/资源管理器显示用;GNU 链路走 windres,UTF-8 安全)
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "CF Hosts GUI · Cloudflare 优选 IP 管理");
        res.set("ProductName", "cf-hosts-gui");
        res.compile().expect("embed windows resource");
    }
}
