fn main() {
    tauri_build::build();
    // 集成测试程序（tests/updater_fallback.rs）也要依赖 Common Controls v6，与应用清单相同：开了
    // tauri 的 `test` 特性后，测试程序会导入 comctl32 v6 才有的函数，没有清单时 Windows 上启动即
    // 失败（STATUS_ENTRYPOINT_NOT_FOUND）。`-tests` 只作用于集成测试，不影响应用本身的清单。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-tests=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
}
