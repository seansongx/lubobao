fn main() {
    // 让 FFmpeg 组件按 Rust 编译目标匹配清单中的 `rustTargets`。
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=LUBOBAO_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
