use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/main");
    println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    let build = env::var("GITHUB_RUN_NUMBER").unwrap_or_else(|_| "local".into());
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    println!("cargo:rustc-env=SNAPBAR_BUILD_ID={build}-{commit}-{timestamp}");
    println!("cargo:rerun-if-changed=assets/branding/snapbar.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
        .join("assets/branding/snapbar.ico");
    let resource = PathBuf::from(env::var_os("OUT_DIR").expect("build output directory"))
        .join("snapbar-icons.rc");
    // Keep GPUI's manifest intact; this resource contains only our app icon.
    fs::write(
        &resource,
        format!(
            "101 ICON \"{}\"\n",
            icon.display().to_string().replace('\\', "/")
        ),
    )
    .expect("write icon resource");
    embed_resource::compile(resource, embed_resource::NONE)
        .manifest_required()
        .expect("compile Snapbar icon resource with the Windows SDK");
}
