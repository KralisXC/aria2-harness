//! 构建脚本：把 `assets/app.ico` 编译进 exe 的 Windows 资源段。
//!
//! 背景：`with_window_icon` / `set_taskbar_icon` 只影响「窗口」图标的运行时行为，
//! 任务栏分组、Alt-Tab、资源管理器与任务管理器读取的是 **exe 文件自身**的图标。
//! 未嵌入资源时，这些入口会回退到系统默认的空白图标。
//!
//! 工具链：不依赖 MSVC 的 `rc.exe`（本机未安装 Windows SDK 资源编译器），
//! 改用 MSYS2 自带的 GNU `windres`（`--output-format=coff` 产出的 COFF 对象
//! 可同时被 MSVC `link.exe` 与 rust-lld 接受）。
//! 找不到 `windres` 时只告警、不失败 —— 不阻断构建，只是 exe 没有图标。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-env-changed=WINDRES");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let rc_file = manifest_dir.join("assets").join("app.rc");
    let obj = out_dir.join("app_res.o");

    let Some(windres) = find_windres() else {
        println!("cargo:warning=未找到 windres（MSYS2 mingw64/bin），跳过 exe 图标资源嵌入");
        return;
    };

    // windres 会调用同目录的 cpp/gcc 预处理 .rc；把它的目录放到 PATH 最前。
    if let Some(dir) = windres.parent() {
        let mut dirs = vec![dir.to_path_buf()];
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        if let Ok(joined) = std::env::join_paths(dirs) {
            // 构建脚本单线程执行，此处设置环境变量仅影响随后启动的 windres 子进程。
            unsafe { std::env::set_var("PATH", joined) };
        }
    }

    let status = Command::new(&windres)
        .arg("--input")
        .arg(&rc_file)
        .arg("--output")
        .arg(&obj)
        .arg("--include-dir")
        .arg(manifest_dir.join("assets"))
        .arg("--output-format=coff")
        .arg(format!("--target={}", windres_target()))
        .status();

    match status {
        Ok(s) if s.success() => {
            // 只给 GUI 主程序挂图标；service 不需要，也避免多 exe 重复链接。
            println!("cargo:rustc-link-arg-bin=aria2-harness={}", obj.display());
        }
        Ok(s) => println!("cargo:warning=windres 退出码 {s}，exe 图标未嵌入"),
        Err(err) => println!("cargo:warning=无法执行 windres（{err}），exe 图标未嵌入"),
    }
}

/// 显式指定资源目标架构，避免 windres 按宿主默认值猜错。
fn windres_target() -> &'static str {
    match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "pe-aarch64-little",
        Ok("x86") => "pe-i386",
        _ => "pe-x86-64",
    }
}

/// 依次尝试：`WINDRES` 环境变量 → PATH → 常见 MSYS2 安装位置。
fn find_windres() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WINDRES") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for name in ["windres.exe", "llvm-windres.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    [
        r"D:\msys2\mingw64\bin\windres.exe",
        r"C:\msys64\mingw64\bin\windres.exe",
        r"C:\msys64\ucrt64\bin\windres.exe",
        r"C:\msys64\clang64\bin\llvm-windres.exe",
    ]
    .iter()
    .map(Path::new)
    .find(|p| p.is_file())
    .map(Path::to_path_buf)
}
