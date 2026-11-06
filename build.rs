fn main() {
    // MALIANG_SKIP_RES=1 时跳过资源嵌入:测试/受限环境(rc.exe 发现机制
    // 依赖注册表查询)下不需要它,只影响成品 exe 的图标,不影响逻辑。
    let skip = std::env::var_os("MALIANG_SKIP_RES").is_some();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") && !skip {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/maliang.ico");
        res.compile().expect("嵌入 Windows 图标资源失败");
    }
    println!("cargo:rerun-if-changed=assets/maliang.ico");
    println!("cargo:rerun-if-changed=assets/icon.png");
}
