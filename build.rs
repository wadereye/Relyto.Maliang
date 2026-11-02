fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/maliang.ico");
        res.compile().expect("嵌入 Windows 图标资源失败");
    }
    println!("cargo:rerun-if-changed=assets/maliang.ico");
    println!("cargo:rerun-if-changed=assets/icon.png");
}
