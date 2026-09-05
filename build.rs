use std::{env, fs, path::PathBuf};

fn main() {
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
