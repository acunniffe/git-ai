use std::fs;
use std::path::Path;

fn repo_file(path: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "ENG-218 requires packaging artifact {}: {error}",
            path.display()
        )
    })
}

fn workflow_job(workflow: &str, job_name: &str) -> String {
    let job_header = format!("  {job_name}:");
    let mut lines = workflow.lines().skip_while(|line| *line != job_header);
    assert_eq!(
        lines.next(),
        Some(job_header.as_str()),
        "ENG-218 requires the {job_name} release job"
    );

    let mut job = String::new();
    for line in lines {
        if line.starts_with("  ") && !line.starts_with("    ") && line.ends_with(':') {
            break;
        }
        job.push_str(line);
        job.push('\n');
    }
    job
}

fn job_needs(job: &str) -> &str {
    job.lines()
        .find(|line| line.trim_start().starts_with("needs:"))
        .expect("release job must declare its dependencies")
}

#[test]
fn eng_218_packaging_sources_enforce_per_user_runtime_boundaries() {
    let readme = repo_file("packaging/README.md");
    assert!(readme.contains("install `git-ai` only"));
    assert!(readme.contains("must not install a `git`"));
    assert!(readme.contains("per-user"));

    let wix = repo_file("packaging/windows/git-ai.wxs");
    for contract in [
        "Scope=\"perUser\"",
        "WIX_DIR_PROFILE",
        "System=\"no\"",
        "Impersonate=\"yes\"",
        "HideTarget=\"yes\"",
        "install-hooks --api-base",
    ] {
        assert!(
            wix.contains(contract),
            "Windows installer must preserve {contract}"
        );
    }
    assert!(!wix.contains("ProgramFilesFolder"));

    let windows_builder = repo_file("packaging/windows/build-msi.ps1");
    assert!(windows_builder.contains("ValidateSet('x64', 'arm64')"));
    assert!(windows_builder.contains("WixToolset.Util.wixext"));

    let macos_builder = repo_file("packaging/macos/build-pkg.sh");
    for contract in ["x64|arm64", "pkgbuild", "productsign"] {
        assert!(
            macos_builder.contains(contract),
            "macOS builder must preserve {contract}"
        );
    }

    let postinstall = repo_file("packaging/macos/scripts/postinstall");
    for contract in [
        "/dev/console",
        "NFSHomeDirectory",
        ".git-ai",
        "/usr/bin/su -",
        "install-hooks",
    ] {
        assert!(
            postinstall.contains(contract),
            "macOS installer must preserve {contract}"
        );
    }
    assert!(!postinstall.contains("/usr/local/bin"));
    assert!(!postinstall.contains("/opt/git-ai"));
}

#[test]
fn eng_218_release_workflow_signs_tests_and_gates_native_packages() {
    let workflow = repo_file(".github/workflows/release.yml");

    let package_msi = workflow_job(&workflow, "package-msi");
    for contract in [
        "id-token: write",
        "azure/login@",
        "azure/artifact-signing-action@",
        "git-ai-windows-x64.msi",
        "git-ai-windows-arm64.msi",
    ] {
        assert!(
            package_msi.contains(contract),
            "MSI packaging job must preserve {contract}"
        );
    }

    let package_pkg = workflow_job(&workflow, "package-pkg");
    for contract in [
        "APPLE_DEVELOPER_ID_INSTALLER",
        "notarytool submit",
        "stapler validate",
        "PKG-SHA256SUMS",
    ] {
        assert!(
            package_pkg.contains(contract),
            "PKG packaging job must preserve {contract}"
        );
    }

    let test_msi = workflow_job(&workflow, "test-msi");
    for contract in [
        "msiexec.exe",
        "config.api_base_url",
        "NT AUTHORITY\\SYSTEM",
        "$env:ProgramFiles",
    ] {
        assert!(
            test_msi.contains(contract),
            "MSI smoke test must preserve {contract}"
        );
    }

    let test_pkg = workflow_job(&workflow, "test-pkg");
    for contract in [
        "sudo installer -pkg",
        "binary_owner",
        "state_owner",
        "/opt/git-ai",
        "/usr/local/bin/git-ai",
    ] {
        assert!(
            test_pkg.contains(contract),
            "PKG smoke test must preserve {contract}"
        );
    }

    let create_release = workflow_job(&workflow, "create-release");
    let create_release_needs = job_needs(&create_release);
    assert!(create_release_needs.contains("package-msi"));
    assert!(create_release_needs.contains("test-msi"));
    assert!(!create_release_needs.contains("package-pkg"));
    assert!(!create_release_needs.contains("test-pkg"));
    assert!(create_release.contains("actions/attest-build-provenance@"));
    assert!(create_release.contains("SHA256SUMS"));

    let publish_pkg = workflow_job(&workflow, "publish-pkg");
    let publish_pkg_needs = job_needs(&publish_pkg);
    for dependency in ["create-release", "package-pkg", "test-pkg"] {
        assert!(
            publish_pkg_needs.contains(dependency),
            "PKG publication must wait for {dependency}"
        );
    }
    assert!(publish_pkg.contains("actions/attest-build-provenance@"));
    assert!(publish_pkg.contains("gh release upload"));
}
