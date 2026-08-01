use crate::error::GitAiError;
use crate::operations::mdm::editor_extension::{
    ExtensionInstallOutcome, ExtensionInstallPresentation, extension_install_result,
};

const CURSOR_EXTENSION_PRESENTATION: ExtensionInstallPresentation = ExtensionInstallPresentation {
    product_name: "Cursor",
    installed_message: "\tExtension 'git-ai.git-ai-vscode' was successfully installed.",
    install_failure_instructions: "Unable to automatically install extension. Please cmd+click on the following link to install: cursor:extension/git-ai.git-ai-vscode (or search for 'git-ai-vscode' in the Cursor extensions tab)",
};
const VSCODE_EXTENSION_PRESENTATION: ExtensionInstallPresentation = ExtensionInstallPresentation {
    product_name: "VS Code",
    installed_message: "VS Code: Extension installed",
    install_failure_instructions: "Unable to automatically install extension. Please cmd+click on the following link to install: vscode:extension/git-ai.git-ai-vscode (or navigate to https://marketplace.visualstudio.com/items?itemName=git-ai.git-ai-vscode in your browser)",
};
const WINDSURF_EXTENSION_PRESENTATION: ExtensionInstallPresentation =
    ExtensionInstallPresentation {
        product_name: "Windsurf",
        installed_message: "\tExtension 'git-ai.git-ai-vscode' was successfully installed.",
        install_failure_instructions: "Unable to automatically install extension. Please cmd+click on the following link to install: windsurf:extension/git-ai.git-ai-vscode (or search for 'git-ai-vscode' in the Windsurf extensions tab)",
    };

fn assert_extension_install_result(
    outcome: ExtensionInstallOutcome,
    presentation: &ExtensionInstallPresentation,
    expected_changed: bool,
    expected_message: &str,
) {
    let result = extension_install_result(outcome, presentation).expect("expected a result");
    assert_eq!(result.changed, expected_changed);
    assert!(result.diff.is_none());
    assert_eq!(result.message, expected_message);
}

#[test]
fn extension_install_result_handles_every_outcome() {
    assert!(
        extension_install_result(
            ExtensionInstallOutcome::CliUnavailable,
            &CURSOR_EXTENSION_PRESENTATION,
        )
        .is_none()
    );
    assert_extension_install_result(
        ExtensionInstallOutcome::AlreadyInstalled,
        &CURSOR_EXTENSION_PRESENTATION,
        false,
        "Cursor: Extension already installed",
    );
    assert_extension_install_result(
        ExtensionInstallOutcome::PendingInstall,
        &CURSOR_EXTENSION_PRESENTATION,
        true,
        "Cursor: Pending extension install",
    );
    assert_extension_install_result(
        ExtensionInstallOutcome::Installed,
        &CURSOR_EXTENSION_PRESENTATION,
        true,
        "\tExtension 'git-ai.git-ai-vscode' was successfully installed.",
    );
    assert_extension_install_result(
        ExtensionInstallOutcome::CheckFailed(GitAiError::Generic("boom".to_string())),
        &CURSOR_EXTENSION_PRESENTATION,
        false,
        "Cursor: Failed to check extension: Generic error: boom",
    );
    assert_extension_install_result(
        ExtensionInstallOutcome::InstallFailed(GitAiError::Generic("boom".to_string())),
        &CURSOR_EXTENSION_PRESENTATION,
        false,
        "Cursor: Unable to automatically install extension. Please cmd+click on the following link to install: cursor:extension/git-ai.git-ai-vscode (or search for 'git-ai-vscode' in the Cursor extensions tab)",
    );
}

#[test]
fn extension_install_result_uses_each_product_presentation() {
    assert_extension_install_result(
        ExtensionInstallOutcome::Installed,
        &VSCODE_EXTENSION_PRESENTATION,
        true,
        "VS Code: Extension installed",
    );
    assert_extension_install_result(
        ExtensionInstallOutcome::InstallFailed(GitAiError::Generic("boom".to_string())),
        &VSCODE_EXTENSION_PRESENTATION,
        false,
        "VS Code: Unable to automatically install extension. Please cmd+click on the following link to install: vscode:extension/git-ai.git-ai-vscode (or navigate to https://marketplace.visualstudio.com/items?itemName=git-ai.git-ai-vscode in your browser)",
    );
    assert_extension_install_result(
        ExtensionInstallOutcome::AlreadyInstalled,
        &WINDSURF_EXTENSION_PRESENTATION,
        false,
        "Windsurf: Extension already installed",
    );
    assert_extension_install_result(
        ExtensionInstallOutcome::InstallFailed(GitAiError::Generic("boom".to_string())),
        &WINDSURF_EXTENSION_PRESENTATION,
        false,
        "Windsurf: Unable to automatically install extension. Please cmd+click on the following link to install: windsurf:extension/git-ai.git-ai-vscode (or search for 'git-ai-vscode' in the Windsurf extensions tab)",
    );
}
