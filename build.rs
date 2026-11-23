use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let executable = env!("CARGO_PKG_NAME");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("app.manifest");
    println!("cargo::rerun-if-changed={}", manifest.display());
    println!("cargo::rustc-link-arg-bin={executable}=/MANIFEST:EMBED");
    println!("cargo::rustc-link-arg-bin={executable}=/MANIFESTUAC:NO");
    println!(
        "cargo::rustc-link-arg-bin={executable}=/MANIFESTINPUT:{}",
        manifest.display()
    );
    compile_resources();
}

fn compile_resources() {
    println!("cargo::rerun-if-changed=app.rc");
    println!("cargo::rerun-if-changed=assets/window-gestures.ico");
    let host = env::var("HOST").expect("Cargo sets HOST");
    let arch = host.split('-').next().expect("host architecture");
    let sdk = find_msvc_tools::find_windows_sdk(arch).expect("find the Windows SDK");
    let compiler = sdk
        .path()
        .map(|path| path.join("rc.exe"))
        .find(|path| path.is_file())
        .expect("find rc.exe in the Windows SDK");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR")).join("app.res");
    let mut command = Command::new(compiler);
    command.arg("/nologo").arg("/fo").arg(&output);
    command.args([
        "/d",
        concat!(
            "APP_VERSION_NUMERIC=",
            env!("CARGO_PKG_VERSION_MAJOR"),
            ",",
            env!("CARGO_PKG_VERSION_MINOR"),
            ",",
            env!("CARGO_PKG_VERSION_PATCH"),
            ",0"
        ),
        "/d",
        concat!("APP_VERSION_STRING=\"", env!("CARGO_PKG_VERSION"), "\""),
    ]);
    for include in sdk.include() {
        command.arg("/I").arg(include);
    }
    assert!(
        command
            .arg("app.rc")
            .status()
            .expect("run rc.exe")
            .success(),
        "compile the Windows resources"
    );
    println!("cargo::rustc-link-arg={}", output.display());
}
