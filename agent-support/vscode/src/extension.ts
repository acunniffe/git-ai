import * as vscode from "vscode";
import { AIEditManager } from "./ai-edit-manager";
import { detectIDEHost, IDEHostKindVSCode } from "./utils/host-kind";
import { AITabEditManager } from "./ai-tab-edit-manager";
import { Config } from "./utils/config";
import { BlameLensManager, registerBlameLensCommands } from "./blame-lens-manager";
import { initBinaryResolver } from "./utils/binary-path";
import { KnownHumanCheckpointManager } from "./known-human-checkpoint-manager";

export function activate(context: vscode.ExtensionContext) {

  // In dev mode, resolve git-ai binary via login shell (debug host has stripped PATH)
  initBinaryResolver(context.extensionMode);

  const ideHostCfg = detectIDEHost();

  const aiEditManager = new AIEditManager(context);

  const knownHumanManager = new KnownHumanCheckpointManager(
    vscode.version,
    context.extension.packageJSON.version,
  );
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument((doc) => {
      knownHumanManager.handleSaveEvent(doc);
    }),
    { dispose: () => knownHumanManager.dispose() },
  );

  // Initialize and activate blame lens manager
  registerBlameLensCommands(context);
  const blameLensManager = new BlameLensManager(context);
  blameLensManager.activate();
  context.subscriptions.push({
    dispose: () => blameLensManager.dispose()
  });

  if (Config.isAiTabTrackingEnabled()) {
    const aiTabEditManager = new AITabEditManager(context, ideHostCfg, aiEditManager);
    const aiTabTrackingEnabled = aiTabEditManager.enableIfSupported();

    if (aiTabTrackingEnabled) {
      console.log('[git-ai] Tracking document content changes for AI tab completion detection');
      vscode.window.showInformationMessage('git-ai: AI tab tracking is enabled (experimental)');
      context.subscriptions.push(
        vscode.workspace.onDidChangeTextDocument((event) => {
          aiTabEditManager.handleDocumentContentChangeEvent(event);
        })
      );
    }
  }

  if (ideHostCfg.kind === IDEHostKindVSCode) {
    if (aiEditManager.areLegacyCopilotHooksEnabled()) {
      console.log('[git-ai] Using VS Code/Copilot legacy extension detection strategy');

      // Save event
      context.subscriptions.push(
        vscode.workspace.onDidSaveTextDocument((doc) => {
          aiEditManager.handleSaveEvent(doc);
        })
      );

      // Open event
      context.subscriptions.push(
        vscode.workspace.onDidOpenTextDocument((doc) => {
          aiEditManager.handleOpenEvent(doc);
        })
      );

      // Close event
      context.subscriptions.push(
        vscode.workspace.onDidCloseTextDocument((doc) => {
          aiEditManager.handleCloseEvent(doc);
        })
      );

      // Content change event (for stable content cache)
      context.subscriptions.push(
        vscode.workspace.onDidChangeTextDocument((event) => {
          aiEditManager.handleContentChangeEvent(event);
        })
      );
    } else {
      console.log('[git-ai] VS Code has native Copilot hooks; skipping legacy extension checkpoint listeners');
    }
  }

  // vscode.commands.getCommands(true)
  //   .then(commands => {
  //     const content = commands.join('\n');
  //     vscode.workspace.openTextDocument({ content, language: 'text' })
  //       .then(doc => vscode.window.showTextDocument(doc));
  //   });
}

export function deactivate() {
  console.log('[git-ai] extension deactivated');
}
