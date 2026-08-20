use std::fs;
use std::path::PathBuf;

fn wix_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("packaging")
        .join("windows")
        .join("git-ai.wxs");
    fs::read_to_string(path).expect("read Windows MSI source")
}

#[test]
fn msi_enables_wsl_installation_by_default() {
    let wix = wix_source();

    assert!(
        wix.contains(r#"<Property Id="INSTALL_WSL" Value="1" Secure="yes" />"#),
        "MSI should define INSTALL_WSL=1 as a secure public property"
    );
    assert!(
        wix.contains(r#"Value="--wsl""#),
        "MSI should translate the enabled property into the CLI flag"
    );
    assert!(
        wix.contains(r#"INSTALL_WSL = &quot;1&quot;"#),
        "MSI should only add --wsl when INSTALL_WSL is enabled"
    );
}

#[test]
fn msi_passes_the_conditional_wsl_argument_to_install_hooks() {
    let wix = wix_source();

    assert!(
        wix.contains("install-hooks --env [WslInstallArgument] --api-base"),
        "MSI configure action should include the conditional WSL argument"
    );
}
