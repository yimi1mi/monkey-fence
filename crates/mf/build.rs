//! 把应用图标与版本信息嵌入 Windows exe 资源。
//! 图标必须使用资源 ID 1:gpui(gpui_windows)启动时用 LoadImageW 加载 ID 1
//! 作为窗口类图标(标题栏 / 任务栏 / Alt-Tab)。

use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../../assets/icon/png/icon-a-robot-dag.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    if env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }

    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut file_version = String::new();
    let mut parts = pkg_version
        .split('.')
        .map(|p| p.parse::<u16>().unwrap_or(0));
    for i in 0..4 {
        let part = parts.next().unwrap_or(0);
        if i > 0 {
            write!(file_version, ",").unwrap();
        }
        write!(file_version, "{part}").unwrap();
    }

    let icon_escaped = icon.to_string_lossy().replace('\\', "\\\\");
    let rc = format!(
        r#"1 ICON "{icon_escaped}"

1 VERSIONINFO
FILEVERSION {file_version}
PRODUCTVERSION {file_version}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "MonkeyFence\0"
            VALUE "ProductName", "MonkeyFence\0"
            VALUE "FileVersion", "{pkg_version}\0"
            VALUE "ProductVersion", "{pkg_version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc_path = out_dir.join("mf_resources.rc");
    std::fs::write(&rc_path, rc).expect("write rc file");

    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .expect("compile Windows resources");
}
