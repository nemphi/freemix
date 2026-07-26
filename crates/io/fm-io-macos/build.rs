use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=native/CameraHelper.swift");
    println!("cargo:rerun-if-changed=native/Info.plist");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

    if env::var_os("CARGO_CFG_TARGET_OS").as_deref() != Some(OsStr::new("macos")) {
        return;
    }

    let target = env::var("TARGET").unwrap_or_else(|_| panic!("Cargo did not set required TARGET"));
    let architecture = match target.as_str() {
        "aarch64-apple-darwin" => "arm64",
        "x86_64-apple-darwin" => "x86_64",
        _ => panic!(
            "unsupported macOS target '{target}'; fm-io-macos supports only aarch64-apple-darwin and x86_64-apple-darwin"
        ),
    };
    let deployment_target = deployment_target();
    let swift_target = format!("{architecture}-apple-macosx{deployment_target}");
    let manifest_dir = required_path("CARGO_MANIFEST_DIR");
    let out_dir = required_path("OUT_DIR");
    let source = manifest_dir.join("native/CameraHelper.swift");
    let info_plist = manifest_dir.join("native/Info.plist");
    let helper = out_dir.join("freemix-camera-helper");
    let module_cache = out_dir.join("swift-module-cache");

    fs::create_dir_all(&module_cache).unwrap_or_else(|error| {
        panic!(
            "failed to create Swift module cache at {}: {error}",
            module_cache.display()
        )
    });

    let output = Command::new("xcrun")
        .arg("--sdk")
        .arg("macosx")
        .arg("swiftc")
        .arg("-O")
        .arg("-target")
        .arg(&swift_target)
        .arg("-module-cache-path")
        .arg(&module_cache)
        .arg(&source)
        .arg("-o")
        .arg(&helper)
        .arg("-Xlinker")
        .arg("-sectcreate")
        .arg("-Xlinker")
        .arg("__TEXT")
        .arg("-Xlinker")
        .arg("__info_plist")
        .arg("-Xlinker")
        .arg(&info_plist)
        .arg("-framework")
        .arg("AVFoundation")
        .arg("-framework")
        .arg("AudioToolbox")
        .arg("-framework")
        .arg("CoreMedia")
        .arg("-framework")
        .arg("CoreVideo")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "failed to invoke xcrun to compile {}: {error}. Install or select Xcode Command Line Tools (for example, run `xcode-select --install`)",
                source.display()
            )
        });

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "Swift camera helper compilation failed with status {}.\ncommand: xcrun --sdk macosx swiftc -O -target {} -module-cache-path {} {} -o {} -Xlinker -sectcreate -Xlinker __TEXT -Xlinker __info_plist -Xlinker {} -framework AVFoundation -framework AudioToolbox -framework CoreMedia -framework CoreVideo\nstdout:\n{}\nstderr:\n{}\nVerify that Xcode Command Line Tools and the macOS SDK are installed and selected",
            output.status,
            swift_target,
            module_cache.display(),
            source.display(),
            helper.display(),
            info_plist.display(),
            stdout,
            stderr
        );
    }

    let helper = helper.canonicalize().unwrap_or_else(|error| {
        panic!(
            "Swift compiler reported success but the camera helper at {} could not be resolved: {error}",
            helper.display()
        )
    });
    println!("cargo:rustc-env=FREEMIX_CAMERA_HELPER={}", helper.display());
}

fn required_path(name: &str) -> PathBuf {
    Path::new(&env::var_os(name).unwrap_or_else(|| panic!("Cargo did not set required {name}")))
        .to_path_buf()
}

fn deployment_target() -> String {
    let value = match env::var("MACOSX_DEPLOYMENT_TARGET") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return "13.0".to_owned(),
        Err(env::VarError::NotUnicode(_)) => {
            panic!("MACOSX_DEPLOYMENT_TARGET must be valid UTF-8")
        }
    };
    let components = value.split('.').collect::<Vec<_>>();
    let valid = (1..=3).contains(&components.len())
        && components.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        });
    assert!(
        valid,
        "invalid MACOSX_DEPLOYMENT_TARGET '{value}'; expected a numeric macOS version such as 13.0"
    );
    let parsed = components
        .iter()
        .map(|component| component.parse::<u32>())
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|_| {
            panic!("invalid MACOSX_DEPLOYMENT_TARGET '{value}'; version component is too large")
        });
    assert!(
        parsed[0] >= 13,
        "MACOSX_DEPLOYMENT_TARGET '{value}' is below the required fm-io-macos minimum of 13.0"
    );
    value
}
