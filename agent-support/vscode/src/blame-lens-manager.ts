import * as vscode from "vscode";
import { BlameService, BlameResult, BlameMetadata, LineBlameInfo } from "./blame-service";
import { Config, BlameMode } from "./utils/config";
import { findRepoForFile } from "./utils/git-api";
import { resolveGitAiBinary } from "./utils/binary-path";

export class BlameLensManager {
  private context: vscode.ExtensionContext;
  private blameService: BlameService;
  private currentBlameResult: BlameResult | null = null;
  private currentDocumentUri: string | null = null;
  private pendingBlameRequest: Promise<BlameResult | null> | null = null;
  private statusBarItem: vscode.StatusBarItem;
  
  // Current blame mode (persisted via settings)
  private blameMode: BlameMode = 'off';
  
  // Track notification timeout to prevent stacking
  private notificationTimeout: NodeJS.Timeout | null = null;
  
  // Virtual document provider for markdown content
  private static readonly VIRTUAL_SCHEME = 'git-ai-blame';
  private markdownContentStore: Map<string, string> = new Map();
  private _onDidChangeVirtualDocument: vscode.EventEmitter<vscode.Uri> = new vscode.EventEmitter<vscode.Uri>();
  
  // Decoration types for colored borders (one per color)
  private colorDecorations: vscode.TextEditorDecorationType[] = [];
  
  // Filtered colors that have sufficient contrast against the current theme
  private filteredColors: string[] = [];
  
  // After-text decoration for showing "[View $MODEL Thread]" on AI lines
  private afterTextDecoration: vscode.TextEditorDecorationType | null = null;

  // Track in-flight CAS prompt fetches to avoid duplicate requests
  private casFetchInProgress: Set<string> = new Set();
  
  // Minimum contrast ratio for WCAG AA compliance (3:1 for UI elements)
  private static readonly MIN_CONTRAST_RATIO = 3.0;
  
  // 40 readable colors for AI hunks
  private readonly HUNK_COLORS = [
    'rgba(96, 165, 250, 0.8)',   // Blue
    'rgba(167, 139, 250, 0.8)',  // Purple
    'rgba(251, 146, 60, 0.8)',   // Orange
    'rgba(244, 114, 182, 0.8)',  // Pink
    'rgba(250, 204, 21, 0.8)',   // Yellow
    'rgba(56, 189, 248, 0.8)',   // Sky Blue
    'rgba(249, 115, 22, 0.8)',   // Deep Orange
    'rgba(168, 85, 247, 0.8)',   // Violet
    'rgba(236, 72, 153, 0.8)',   // Hot Pink
    'rgba(148, 163, 184, 0.8)',  // Cool Gray
    'rgba(59, 130, 246, 0.8)',   // Bright Blue
    'rgba(139, 92, 246, 0.8)',   // Purple Violet
    'rgba(234, 179, 8, 0.8)',    // Gold
    'rgba(236, 72, 85, 0.8)',    // Red
    'rgba(20, 184, 166, 0.8)',   // Teal
    'rgba(251, 191, 36, 0.8)',   // Amber
    'rgba(192, 132, 252, 0.8)',  // Light Purple
    'rgba(147, 197, 253, 0.8)',  // Light Blue
    'rgba(252, 165, 165, 0.8)',  // Light Red
    'rgba(134, 239, 172, 0.8)',  // Light Green
    'rgba(253, 224, 71, 0.8)',   // Bright Yellow
    'rgba(165, 180, 252, 0.8)',  // Indigo
    'rgba(253, 186, 116, 0.8)',  // Light Orange
    'rgba(249, 168, 212, 0.8)',  // Light Pink
    'rgba(94, 234, 212, 0.8)',   // Cyan
    'rgba(199, 210, 254, 0.8)',  // Pale Indigo
    'rgba(254, 240, 138, 0.8)',  // Pale Yellow
    'rgba(191, 219, 254, 0.8)',  // Pale Blue
    'rgba(254, 202, 202, 0.8)',  // Pale Red
    'rgba(187, 247, 208, 0.8)',  // Pale Green
    'rgba(167, 243, 208, 0.8)',  // Pale Teal
    'rgba(253, 230, 138, 0.8)',  // Pale Amber
    'rgba(216, 180, 254, 0.8)',  // Pale Purple
    'rgba(254, 215, 170, 0.8)',  // Pale Orange
    'rgba(251, 207, 232, 0.8)',  // Pale Pink
    'rgba(129, 140, 248, 0.8)',  // Medium Indigo
    'rgba(248, 113, 113, 0.8)',  // Medium Red
    'rgba(255, 112, 67, 0.8)',   // Coral
    'rgba(45, 212, 191, 0.8)',   // Medium Teal
    'rgba(251, 146, 189, 0.8)',  // Medium Pink
  ];

  /**
   * Create a tiny SVG data URI that draws a solid stripe in the requested color.
   * The gutter icon is reused per decoration to keep the stripe inside the gutter.
   */
  private createGutterIconUri(color: string): vscode.Uri {
    const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="4" height="24"><rect width="4" height="24" fill="${color}" /></svg>`;
    return vscode.Uri.parse(`data:image/svg+xml;utf8,${encodeURIComponent(svg)}`);
  }

  constructor(context: vscode.ExtensionContext) {
    this.context = context;
    this.blameService = new BlameService();
    
    // Initialize from global setting
    this.blameMode = Config.getBlameMode();

    // Initialize filtered colors and create decoration types based on theme contrast
    this.rebuildColorDecorations();

    // Create status bar item for AI mode toggle and model display
    this.statusBarItem = vscode.window.createStatusBarItem(
      vscode.StatusBarAlignment.Right,
      500
    );
    this.statusBarItem.name = 'git-ai';
    this.statusBarItem.command = 'git-ai.toggleAICode';
    this.statusBarItem.text = '🧑‍💻';
    this.statusBarItem.tooltip = 'Human-authored code (click to toggle AI highlighting)';
    // Status bar starts hidden - only shown after blame loads
  }

  public activate(): void {
    // Register virtual document provider for markdown content
    const documentProvider = new class implements vscode.TextDocumentContentProvider {
      constructor(private manager: BlameLensManager) {}
      
      provideTextDocumentContent(uri: vscode.Uri): string {
        // Extract content ID from path (remove leading / and trailing extension like .md or .diff)
        const contentId = uri.path.replace(/^\//, '').replace(/\.(md|diff)$/, '');
        const content = this.manager.markdownContentStore.get(contentId);
        if (!content) {
          return '// Content not found';
        }
        return content;
      }
      
      get onDidChange(): vscode.Event<vscode.Uri> {
        return this.manager._onDidChangeVirtualDocument.event;
      }
    }(this);
    
    this.context.subscriptions.push(
      vscode.workspace.registerTextDocumentContentProvider(BlameLensManager.VIRTUAL_SCHEME, documentProvider)
    );

    // Register selection/cursor change listener to update status bar
    this.context.subscriptions.push(
      vscode.window.onDidChangeTextEditorSelection((event) => {
        this.handleSelectionChange(event);
      })
    );

    // Handle tab/document close to cancel pending blames
    this.context.subscriptions.push(
      vscode.workspace.onDidCloseTextDocument((document) => {
        this.handleDocumentClose(document);
      })
    );

    // Handle active editor change to update status bar and decorations
    this.context.subscriptions.push(
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        this.handleActiveEditorChange(editor);
      })
    );

    // Handle file save to invalidate cache and potentially refresh blame
    this.context.subscriptions.push(
      vscode.workspace.onDidSaveTextDocument((document) => {
        this.handleDocumentSave(document);
      })
    );

    // Handle document content changes to refresh blame with shifted line attributions
    this.context.subscriptions.push(
      vscode.workspace.onDidChangeTextDocument((event) => {
        this.handleDocumentChange(event);
      })
    );

    // Register command to show commit diff
    this.context.subscriptions.push(
      vscode.commands.registerCommand('git-ai.showCommitDiff', async (commitSha: string, workspacePath: string) => {
        await this.showCommitDiff(commitSha, workspacePath);
      })
    );

    // Register Toggle AI Code command (now shows QuickPick for mode selection)
    this.context.subscriptions.push(
      vscode.commands.registerCommand('git-ai.toggleAICode', () => {
        this.showBlameModeQuickPick();
      })
    );

    // Add status bar item to context subscriptions for proper cleanup
    this.context.subscriptions.push(this.statusBarItem);

    // Listen for configuration changes to blame mode
    this.context.subscriptions.push(
      vscode.workspace.onDidChangeConfiguration((event) => {
        if (event.affectsConfiguration('gitai.blameMode')) {
          this.handleBlameModeChange();
        }
        // Rebuild color decorations if workbench color customizations change
        if (event.affectsConfiguration('workbench.colorCustomizations')) {
          console.log('[git-ai] Color customizations changed, rebuilding color decorations');
          this.rebuildColorDecorations();
        }
      })
    );

    // Listen for theme changes to rebuild color decorations with proper contrast
    this.context.subscriptions.push(
      vscode.window.onDidChangeActiveColorTheme(() => {
        console.log('[git-ai] Theme changed, rebuilding color decorations');
        this.rebuildColorDecorations();
      })
    );

    // Proactively trigger decorations for the already-open editor.
    // VS Code does not reliably fire onDidChangeActiveTextEditor for an
    // editor that is already active when the extension activates.
    // We call requestBlameForFullFile / updateStatusBar directly instead
    // of handleActiveEditorChange to avoid the border-clearing logic
    // which would race with any VS Code activation events.
    const initialEditor = vscode.window.activeTextEditor;
    if (initialEditor && this.blameMode !== 'off') {
      if (this.blameMode === 'all') {
        this.requestBlameForFullFile(initialEditor);
      }
      this.updateStatusBar(initialEditor);
    }

    console.log('[git-ai] BlameLensManager activated');

    // Resolve git-ai binary path early (uses login shell to get full user PATH)
    resolveGitAiBinary().then((path) => {
      if (path) {
        const { execFile } = require('child_process');
        execFile(path, ['--version'], (err: Error | null, stdout: string) => {
          if (!err) {
            console.log('[git-ai] Version:', stdout.trim());
          }
        });
      }
    });
  }

  /**
   * Handle document save - invalidate cache and refresh blame if enabled.
   */
  private handleDocumentSave(document: vscode.TextDocument): void {
    const documentUri = document.uri.toString();
    
    // Invalidate cached blame for this document
    this.blameService.invalidateCache(document.uri);

    // Nothing to do when blame is off
    if (this.blameMode === 'off') {
      return;
    }
    
    // If this is the current document with blame, clear and re-fetch
    if (this.currentDocumentUri === documentUri) {
      this.currentBlameResult = null;
      this.pendingBlameRequest = null;
      this.casFetchInProgress.clear();

      const activeEditor = vscode.window.activeTextEditor;
      if (activeEditor && activeEditor.document.uri.toString() === documentUri) {
        // Re-fetch blame if mode is 'all'
        // Skip clearColoredBorders — applyFullFileDecorations replaces atomically (no flash).
        if (this.blameMode === 'all') {
          this.requestBlameForFullFile(activeEditor);
        }

        // Update status bar
        this.updateStatusBar(activeEditor);
      }
    }

    console.log('[git-ai] Document saved, invalidated blame cache for:', document.uri.fsPath);
  }

  /**
   * Handle document content change - invalidate cached blame and re-fetch with shifted line attributions.
   * This is called on every keystroke, so we debounce the refresh.
   */
  private documentChangeTimer: NodeJS.Timeout | null = null;
  private handleDocumentChange(event: vscode.TextDocumentChangeEvent): void {
    // Nothing to do when blame is off
    if (this.blameMode === 'off') {
      return;
    }

    const documentUri = event.document.uri.toString();

    // Only handle changes to the current document we have blame for
    if (this.currentDocumentUri !== documentUri) {
      return;
    }

    // Skip if no content changes (e.g., just metadata changes)
    if (event.contentChanges.length === 0) {
      return;
    }
    
    // Clear the current blame result since line numbers have shifted
    this.currentBlameResult = null;
    this.pendingBlameRequest = null;
    
    // Debounce the refresh to avoid hammering git-ai on every keystroke
    if (this.documentChangeTimer) {
      clearTimeout(this.documentChangeTimer);
    }
    
    this.documentChangeTimer = setTimeout(() => {
      this.documentChangeTimer = null;

      const activeEditor = vscode.window.activeTextEditor;
      if (activeEditor && activeEditor.document.uri.toString() === documentUri) {
        // Re-fetch blame if mode is 'all'
        // Note: we intentionally do NOT clearColoredBorders() here —
        // applyFullFileDecorations() atomically replaces all decoration types,
        // so old decorations stay visible until new blame arrives (no flash).
        if (this.blameMode === 'all') {
          this.requestBlameForFullFile(activeEditor);
        }

        // Update status bar
        this.updateStatusBar(activeEditor);
      }
    }, 300); // 300ms debounce
  }

  /**
   * Handle document close - cancel any pending blame requests and clean up cache.
   */
  private handleDocumentClose(document: vscode.TextDocument): void {
    const documentUri = document.uri.toString();
    
    // Clear colored borders if this was the current document
    const editor = vscode.window.visibleTextEditors.find(
      e => e.document.uri.toString() === documentUri
    );
    if (editor) {
      this.clearColoredBorders(editor);
    }
    
    // Cancel any pending blame for this document
    this.blameService.cancelForUri(document.uri);
    
    // Clear cached blame result if it matches
    if (this.currentDocumentUri === documentUri) {
      this.currentBlameResult = null;
      this.currentDocumentUri = null;
      this.pendingBlameRequest = null;
    }
    
    // Invalidate cache
    this.blameService.invalidateCache(document.uri);
    
    console.log('[git-ai] Document closed, cancelled blame for:', document.uri.fsPath);
  }

  /**
   * Handle active editor change - update status bar and decorations.
   */
  private handleActiveEditorChange(editor: vscode.TextEditor | undefined): void {
    if (this.blameMode === 'off') {
      return;
    }

    const newDocumentUri = editor?.document.uri.toString() ?? null;

    // Only clear borders and reset state when switching to a different document.
    // Re-firing for the same document (e.g. VS Code activation event) must not
    // clear decorations that were just applied.
    if (newDocumentUri !== this.currentDocumentUri) {
      const previousEditor = vscode.window.visibleTextEditors.find(
        e => e.document.uri.toString() === this.currentDocumentUri
      );
      if (previousEditor) {
        this.clearColoredBorders(previousEditor);
      }

      this.currentBlameResult = null;
      this.currentDocumentUri = null;
      this.pendingBlameRequest = null;
    }

    // If mode is 'all', automatically request blame for the new editor
    if (this.blameMode === 'all' && editor) {
      this.requestBlameForFullFile(editor);
    }

    // Update status bar for the new editor
    this.updateStatusBar(editor);
  }

  /**
   * Show QuickPick dropdown for selecting blame mode.
   */
  private async showBlameModeQuickPick(): Promise<void> {
    const items: vscode.QuickPickItem[] = [
      {
        label: 'Off',
        description: 'No Gutter Annotations for AI Lines',
        picked: this.blameMode === 'off',
      },
      {
        label: 'Line',
        description: 'Show Gutter Annotations for current line\'s prompt',
        picked: this.blameMode === 'line',
      },
      {
        label: 'All',
        description: 'Show Gutter Annotations for all AI-authored lines',
        picked: this.blameMode === 'all',
      },
    ];
    
    const selected = await vscode.window.showQuickPick(items, {
      placeHolder: 'Select Git AI Blame Mode',
      title: 'Git AI Blame Mode',
    });
    
    if (!selected) {
      return; // User cancelled
    }
    
    // Determine selected mode from label
    let newMode: BlameMode;
    if (selected.label === 'Off') {
      newMode = 'off';
    } else if (selected.label === 'Line') {
      newMode = 'line';
    } else {
      newMode = 'all';
    }
    
    // Only update if mode changed
    if (newMode !== this.blameMode) {
      await this.applyBlameMode(newMode);
    }
  }

  /**
   * Apply a new blame mode and persist to settings.
   */
  private async applyBlameMode(newMode: BlameMode): Promise<void> {
    const oldMode = this.blameMode;
    this.blameMode = newMode;

    // Persist to settings
    await Config.setBlameMode(newMode);

    const editor = vscode.window.activeTextEditor;

    // Handle mode transitions
    if (newMode === 'off') {
      // Switching to off: cancel everything and clear all state
      this.pendingBlameRequest = null;
      this.currentBlameResult = null;
      if (this.documentChangeTimer) {
        clearTimeout(this.documentChangeTimer);
        this.documentChangeTimer = null;
      }
      if (editor) {
        this.clearColoredBorders(editor);
      }
      this.statusBarItem.hide();
      this.clearAfterTextDecoration();
    } else if (newMode === 'all') {
      // Switching to all: request full file blame
      if (editor) {
        this.requestBlameForFullFile(editor);
      }
      this.updateStatusBar(editor);
    } else {
      // Switching to line
      if (oldMode === 'all' && editor) {
        this.clearColoredBorders(editor);
      }
      this.updateStatusBar(editor);
    }

    console.log(`[git-ai] Blame mode changed to: ${newMode}`);
  }

  /**
   * Handle blame mode setting change from VS Code settings.
   * This is called when the user changes the setting via Settings UI or settings.json.
   */
  private handleBlameModeChange(): void {
    const newMode = Config.getBlameMode();

    // No change, nothing to do
    if (newMode === this.blameMode) {
      return;
    }

    const oldMode = this.blameMode;
    this.blameMode = newMode;
    const editor = vscode.window.activeTextEditor;

    // Handle mode transitions
    if (newMode === 'off') {
      // Switching to off: cancel everything and clear all state
      this.pendingBlameRequest = null;
      this.currentBlameResult = null;
      if (this.documentChangeTimer) {
        clearTimeout(this.documentChangeTimer);
        this.documentChangeTimer = null;
      }
      if (editor) {
        this.clearColoredBorders(editor);
      }
      this.statusBarItem.hide();
      this.clearAfterTextDecoration();
    } else if (newMode === 'all') {
      // Switching to all: request full file blame
      if (editor) {
        this.requestBlameForFullFile(editor);
      }
      this.updateStatusBar(editor);
    } else {
      // Switching to line
      if (oldMode === 'all' && editor) {
        this.clearColoredBorders(editor);
      }
      this.updateStatusBar(editor);
    }

    console.log(`[git-ai] Blame mode changed to: ${newMode} via settings`);
  }

  /**
   * Request blame for the full file and apply decorations to all AI-authored lines.
   * Used when Toggle AI Code is enabled.
   */
  private async requestBlameForFullFile(editor: vscode.TextEditor): Promise<void> {
    const document = editor.document;
    const documentUri = document.uri.toString();

    // Check if we already have blame for this document
    if (this.currentDocumentUri === documentUri && this.currentBlameResult) {
      this.applyFullFileDecorations(editor, this.currentBlameResult);
      this.updateStatusBar(editor);
      return;
    }

    // Request blame
    try {
      // Cancel any pending request for a different document
      if (this.currentDocumentUri !== documentUri) {
        this.pendingBlameRequest = null;
      }

      // Start new request if not already pending
      if (!this.pendingBlameRequest) {
        this.pendingBlameRequest = this.blameService.requestBlame(document, 'high');
        this.currentDocumentUri = documentUri;
      }

      const result = await this.pendingBlameRequest;
      this.pendingBlameRequest = null;

      if (result) {
        this.currentBlameResult = result;

        // Trigger async CAS fetches for prompts with messages_url but no messages
        this.triggerCASFetches(result, document.uri);

        // Check if editor is still active and mode is still 'all'
        const currentEditor = vscode.window.activeTextEditor;
        if (this.blameMode === 'all' && currentEditor && currentEditor.document.uri.toString() === documentUri) {
          this.applyFullFileDecorations(currentEditor, result);
          this.updateStatusBar(currentEditor);
        }
      }
    } catch (error) {
      console.error('[git-ai] Blame request failed:', error);
      this.pendingBlameRequest = null;
    }
  }

  /**
   * Apply colored borders to ALL AI-authored lines in the entire file.
   * Used when Toggle AI Code is enabled.
   */
  private applyFullFileDecorations(editor: vscode.TextEditor, blameResult: BlameResult): void {
    // Collect all AI-authored lines grouped by color
    const colorToRanges = new Map<number, vscode.Range[]>();

    for (const [gitLine, lineInfo] of blameResult.lineAuthors) {
      if (lineInfo?.isAiAuthored) {
        const colorIndex = this.getColorIndexForPromptId(lineInfo.commitHash);
        const line = gitLine - 1; // Convert to 0-indexed

        if (!colorToRanges.has(colorIndex)) {
          colorToRanges.set(colorIndex, []);
        }
        colorToRanges.get(colorIndex)!.push(new vscode.Range(line, 0, line, 0));
      }
    }

    // Set all decoration types in a single pass: ranges for used colors,
    // empty for unused. Avoids clear-then-set on the same type which
    // VS Code can optimize away when only one decoration type changes.
    this.colorDecorations.forEach((decoration, index) => {
      editor.setDecorations(decoration, colorToRanges.get(index) || []);
    });
  }

  /**
   * Handle cursor/selection change - update status bar to show current line's attribution.
   */
  private handleSelectionChange(event: vscode.TextEditorSelectionChangeEvent): void {
    if (this.blameMode === 'off') {
      return;
    }
    const editor = event.textEditor;
    this.updateStatusBar(editor);
  }

  /**
   * Update status bar based on the current cursor position.
   * Shows model name if the current line is AI-authored, otherwise shows human icon.
   * Text color matches the gutter highlight color for the current prompt.
   */
  private async updateStatusBar(editor: vscode.TextEditor | undefined): Promise<void> {
    if (!editor) {
      this.statusBarItem.hide();
      this.clearAfterTextDecoration();
      return;
    }

    const document = editor.document;
    const documentUri = document.uri.toString();
    const currentLine = editor.selection.active.line;
    const gitLine = currentLine + 1; // Convert to 1-indexed

    // Check if we have blame for this document
    if (this.currentDocumentUri !== documentUri || !this.currentBlameResult) {
      // Don't start a new blame request while we're in a document-change debounce window.
      // The debounce callback will handle it after typing stops.
      if (this.documentChangeTimer) {
        this.statusBarItem.hide();
        this.clearAfterTextDecoration();
        return;
      }

      // Request blame in background if we don't have it
      if (!this.pendingBlameRequest) {
        this.pendingBlameRequest = this.blameService.requestBlame(document, 'normal');
        this.currentDocumentUri = documentUri;
        
        this.pendingBlameRequest.then(result => {
          this.pendingBlameRequest = null;
          if (result) {
            // Bail out if blame was switched off while the request was in flight
            if (this.blameMode === 'off') {
              return;
            }

            this.currentBlameResult = result;

            // Trigger async CAS fetches for prompts with messages_url but no messages
            this.triggerCASFetches(result, document.uri);

            // Re-update status bar now that we have blame
            const activeEditor = vscode.window.activeTextEditor;
            if (activeEditor && activeEditor.document.uri.toString() === documentUri) {
              this.updateStatusBar(activeEditor);
            }
          }
        }).catch(error => {
          console.error('[git-ai] Failed to get blame for status bar:', error);
          this.pendingBlameRequest = null;
        });
      }
      
      // Hide status bar and after-text while loading
      this.statusBarItem.hide();
      this.clearAfterTextDecoration();
      return;
    }

    // Check the current line
    const lineInfo = this.currentBlameResult.lineAuthors.get(gitLine);
    if (lineInfo?.isAiAuthored) {
      const model = lineInfo.promptRecord?.agent_id?.model;
      const tool = lineInfo.promptRecord?.agent_id?.tool || lineInfo.author;
      const modelName = this.extractModelName(model);
      
      // Set status bar color to match gutter highlight
      const colorIndex = this.getColorIndexForPromptId(lineInfo.commitHash);
      const gutterColorHex = this.rgbaToHex(this.filteredColors[colorIndex] || this.HUNK_COLORS[colorIndex]);
      this.statusBarItem.color = gutterColorHex;
      
      // Always show robot emoji for AI code
      // Show model name if available, otherwise show tool name
      if (modelName) {
        this.statusBarItem.text = `🤖 ${modelName}`;
      } else if (tool) {
        // Capitalize tool name
        const toolCapitalized = tool.charAt(0).toUpperCase() + tool.slice(1);
        this.statusBarItem.text = `🤖 ${toolCapitalized}`;
      } else {
        this.statusBarItem.text = '🤖';
      }
      
      // Simple tooltip - no prompt content
      this.statusBarItem.tooltip = 'AI-authored code (click to change mode)';
      
      // Show gutter decorations based on mode
      if (this.blameMode === 'line') {
        this.applyDecorationsForPrompt(editor, lineInfo.commitHash, this.currentBlameResult);
      }
      // Mode 'all' already shows all decorations, mode 'off' shows none
      
      // Show after-text decoration with hover for AI lines
      this.updateAfterTextDecoration(editor, lineInfo, document.uri);
    } else {
      // Show human icon for human-authored code
      this.statusBarItem.text = '🧑‍💻';
      this.statusBarItem.tooltip = 'Human-authored code (click to change mode)';
      this.statusBarItem.color = undefined; // Reset color for human code
      
      // Clear decorations if not on AI line (in 'line' mode)
      if (this.blameMode === 'line') {
        this.clearColoredBorders(editor);
      }
      // Mode 'all' keeps all decorations, mode 'off' already has none
      
      // Clear after-text decoration for human lines
      this.clearAfterTextDecoration();
    }
    
    // Make sure the status bar is visible (may have been hidden during loading)
    this.statusBarItem.show();
  }

  /**
   * Apply gutter decorations for all lines belonging to a specific prompt.
   * Used when cursor is on an AI-authored line to highlight all lines from that prompt.
   */
  private applyDecorationsForPrompt(editor: vscode.TextEditor, commitHash: string, blameResult: BlameResult): void {
    // Get the color for this prompt
    const colorIndex = this.getColorIndexForPromptId(commitHash);
    const ranges: vscode.Range[] = [];

    // Find all lines belonging to this prompt
    for (const [gitLine, lineInfo] of blameResult.lineAuthors) {
      if (lineInfo?.isAiAuthored && lineInfo.commitHash === commitHash) {
        const line = gitLine - 1; // Convert to 0-indexed
        ranges.push(new vscode.Range(line, 0, line, 0));
      }
    }

    // Set all decoration types in a single pass: ranges for this prompt's
    // color, empty for all others. Avoids clear-then-set on the same type.
    this.colorDecorations.forEach((decoration, index) => {
      editor.setDecorations(decoration, index === colorIndex ? ranges : []);
    });
  }

  /**
   * Get a deterministic color index for a prompt ID using hash modulo.
   * This ensures all users see the same color for the same prompt_id.
   * Uses the filtered colors array length for the modulo.
   */
  private getColorIndexForPromptId(promptId: string): number {
    // Simple string hash function
    let hash = 0;
    for (let i = 0; i < promptId.length; i++) {
      hash = ((hash << 5) - hash) + promptId.charCodeAt(i);
      hash = hash & hash; // Convert to 32-bit integer
    }
    // Use filtered colors length (falls back to full palette if empty)
    const colorCount = this.filteredColors.length || this.HUNK_COLORS.length;
    return Math.abs(hash) % colorCount;
  }

  /**
   * Convert rgba color string to hex format for markdown compatibility.
   * Input: 'rgba(96, 165, 250, 0.8)' -> Output: '#60a5fa'
   */
  private rgbaToHex(rgba: string): string {
    const match = rgba.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/);
    if (!match) {
      return '#a78bfa'; // Fallback purple
    }
    const r = parseInt(match[1], 10);
    const g = parseInt(match[2], 10);
    const b = parseInt(match[3], 10);
    return `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${b.toString(16).padStart(2, '0')}`;
  }

  /**
   * Parse an rgba color string to RGB values.
   * Input: 'rgba(96, 165, 250, 0.8)' -> { r: 96, g: 165, b: 250 }
   */
  private parseRgba(rgba: string): { r: number; g: number; b: number } | null {
    const match = rgba.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/);
    if (!match) {
      return null;
    }
    return {
      r: parseInt(match[1], 10),
      g: parseInt(match[2], 10),
      b: parseInt(match[3], 10),
    };
  }

  /**
   * Parse a hex color string to RGB values.
   * Input: '#60a5fa' or '#fff' -> { r: 96, g: 165, b: 250 }
   */
  private hexToRgb(hex: string): { r: number; g: number; b: number } | null {
    // Remove # if present
    const cleanHex = hex.replace(/^#/, '');
    
    // Handle shorthand (e.g., #fff -> #ffffff)
    let fullHex = cleanHex;
    if (cleanHex.length === 3) {
      fullHex = cleanHex.split('').map(c => c + c).join('');
    }
    
    if (fullHex.length !== 6) {
      return null;
    }
    
    const bigint = parseInt(fullHex, 16);
    if (isNaN(bigint)) {
      return null;
    }
    
    return {
      r: (bigint >> 16) & 255,
      g: (bigint >> 8) & 255,
      b: bigint & 255,
    };
  }

  /**
   * Calculate the relative luminance of a color according to WCAG 2.1.
   * https://www.w3.org/TR/WCAG21/#dfn-relative-luminance
   */
  private getRelativeLuminance(r: number, g: number, b: number): number {
    const [rs, gs, bs] = [r, g, b].map(c => {
      const sRGB = c / 255;
      return sRGB <= 0.03928
        ? sRGB / 12.92
        : Math.pow((sRGB + 0.055) / 1.055, 2.4);
    });
    return 0.2126 * rs + 0.7152 * gs + 0.0722 * bs;
  }

  /**
   * Calculate the contrast ratio between two colors according to WCAG 2.1.
   * Returns a value between 1 (no contrast) and 21 (maximum contrast).
   * https://www.w3.org/TR/WCAG21/#dfn-contrast-ratio
   */
  private getContrastRatio(
    color1: { r: number; g: number; b: number },
    color2: { r: number; g: number; b: number }
  ): number {
    const lum1 = this.getRelativeLuminance(color1.r, color1.g, color1.b);
    const lum2 = this.getRelativeLuminance(color2.r, color2.g, color2.b);
    const lighter = Math.max(lum1, lum2);
    const darker = Math.min(lum1, lum2);
    return (lighter + 0.05) / (darker + 0.05);
  }

  /**
   * Get the background colors for the status bar and editor gutter from the current theme.
   * Checks user colorCustomizations first, then falls back to defaults based on theme kind.
   */
  private getThemeBackgroundColors(): { statusBar: { r: number; g: number; b: number }; gutter: { r: number; g: number; b: number } } {
    // Get user's color customizations
    const colorCustomizations = vscode.workspace.getConfiguration('workbench').get<Record<string, string>>('colorCustomizations') || {};
    
    // Get the current theme kind
    const themeKind = vscode.window.activeColorTheme.kind;
    
    // Default colors based on theme kind
    let defaultStatusBar: string;
    let defaultGutter: string;
    
    switch (themeKind) {
      case vscode.ColorThemeKind.Light:
        defaultStatusBar = '#f3f3f3';
        defaultGutter = '#ffffff';
        break;
      case vscode.ColorThemeKind.HighContrastLight:
        defaultStatusBar = '#ffffff';
        defaultGutter = '#ffffff';
        break;
      case vscode.ColorThemeKind.HighContrast:
        defaultStatusBar = '#000000';
        defaultGutter = '#000000';
        break;
      case vscode.ColorThemeKind.Dark:
      default:
        defaultStatusBar = '#1e1e1e';
        defaultGutter = '#1e1e1e';
        break;
    }
    
    // Check for user overrides
    const statusBarColor = colorCustomizations['statusBar.background'] || defaultStatusBar;
    const gutterColor = colorCustomizations['editorGutter.background'] 
      || colorCustomizations['editor.background'] 
      || defaultGutter;
    
    // Parse colors to RGB
    const statusBarRgb = this.hexToRgb(statusBarColor) || this.hexToRgb(defaultStatusBar)!;
    const gutterRgb = this.hexToRgb(gutterColor) || this.hexToRgb(defaultGutter)!;
    
    return { statusBar: statusBarRgb, gutter: gutterRgb };
  }

  /**
   * Filter colors by contrast ratio against the status bar and gutter backgrounds.
   * Returns only colors that have sufficient contrast (3:1 WCAG AA) against both backgrounds.
   */
  private filterColorsByContrast(
    colors: string[],
    backgrounds: { statusBar: { r: number; g: number; b: number }; gutter: { r: number; g: number; b: number } }
  ): string[] {
    return colors.filter(color => {
      const colorRgb = this.parseRgba(color);
      if (!colorRgb) {
        return false;
      }
      
      const statusContrast = this.getContrastRatio(colorRgb, backgrounds.statusBar);
      const gutterContrast = this.getContrastRatio(colorRgb, backgrounds.gutter);
      
      return statusContrast >= BlameLensManager.MIN_CONTRAST_RATIO && 
             gutterContrast >= BlameLensManager.MIN_CONTRAST_RATIO;
    });
  }

  /**
   * Rebuild color decorations based on the filtered colors.
   * Called when theme changes or color customizations change.
   */
  private rebuildColorDecorations(): void {
    // Dispose existing decorations
    this.colorDecorations.forEach(decoration => decoration.dispose());
    
    // Get current theme background colors
    const backgrounds = this.getThemeBackgroundColors();
    
    this.filteredColors = this.filterColorsByContrast(this.HUNK_COLORS, backgrounds);
    
    if (this.filteredColors.length === 0) {
      console.log('[git-ai] All colors filtered out by contrast check, using full palette');
      this.filteredColors = [...this.HUNK_COLORS];
    } else {
      console.log(`[git-ai] Filtered colors: ${this.filteredColors.length}/${this.HUNK_COLORS.length} colors have sufficient contrast`);
    }
    
    // Create new decoration types for each filtered color
    this.colorDecorations = this.filteredColors.map(color => 
      vscode.window.createTextEditorDecorationType({
        isWholeLine: true,
        gutterIconPath: this.createGutterIconUri(color),
        gutterIconSize: 'contain',
        overviewRulerColor: color,
        overviewRulerLane: vscode.OverviewRulerLane.Left,
      })
    );
    
    // Re-apply decorations if mode is 'all'
    if (this.blameMode === 'all') {
      const editor = vscode.window.activeTextEditor;
      if (editor && this.currentBlameResult) {
        this.applyFullFileDecorations(editor, this.currentBlameResult);
      }
    }
  }

  /**
   * Clear all colored border decorations.
   */
  private clearColoredBorders(editor: vscode.TextEditor): void {
    this.colorDecorations.forEach(decoration => {
      editor.setDecorations(decoration, []);
    });
  }

  /**
   * Update the after-text decoration for the current cursor line.
   * Shows "[View $MODEL Thread]" with hover content for AI-authored lines.
   */
  private updateAfterTextDecoration(
    editor: vscode.TextEditor | undefined,
    lineInfo: LineBlameInfo | undefined,
    documentUri?: vscode.Uri
  ): void {
    // Dispose previous decoration since text is dynamic per model
    if (this.afterTextDecoration) {
      this.afterTextDecoration.dispose();
      this.afterTextDecoration = null;
    }

    // Don't show if no editor or line is not AI-authored
    if (!editor || !lineInfo?.isAiAuthored) {
      return;
    }

    // Extract model name for display
    const model = lineInfo.promptRecord?.agent_id?.model;
    const tool = lineInfo.promptRecord?.agent_id?.tool || lineInfo.author;
    const modelName = this.extractModelName(model);
    
    // Build display text: prefer model name, fall back to tool name, then "AI"
    let displayName: string;
    if (modelName) {
      displayName = modelName;
    } else if (tool) {
      displayName = tool.charAt(0).toUpperCase() + tool.slice(1);
    } else {
      displayName = 'AI';
    }

    // Get the color for this prompt (matches gutter stripe)
    const colorIndex = this.getColorIndexForPromptId(lineInfo.commitHash);
    const gutterColorHex = this.rgbaToHex(this.filteredColors[colorIndex] || this.HUNK_COLORS[colorIndex]);

    // Create decoration type with after-text styling
    this.afterTextDecoration = vscode.window.createTextEditorDecorationType({
      after: {
        contentText: ` + ${displayName}`,
        color: gutterColorHex,
        margin: '0 2px 0 0',
        
      },
      overviewRulerLane: vscode.OverviewRulerLane.Left,
    });

    // Build hover content (reuse existing method)
    const hoverContent = this.buildHoverContent(lineInfo, documentUri, this.currentBlameResult ?? undefined);

    // Apply decoration to current line with hover
    const currentLine = editor.selection.active.line;
    const decorationOptions: vscode.DecorationOptions = {
      range: new vscode.Range(currentLine, Number.MAX_SAFE_INTEGER, currentLine, Number.MAX_SAFE_INTEGER),
      hoverMessage: hoverContent,
    };

    editor.setDecorations(this.afterTextDecoration, [decorationOptions]);
  }

  /**
   * Clear the after-text decoration.
   */
  private clearAfterTextDecoration(): void {
    if (this.afterTextDecoration) {
      this.afterTextDecoration.dispose();
      this.afterTextDecoration = null;
    }
  }

  /**
   * Extract just the name from a git author string like "Aidan Cunniffe <acunniffe@gmail.com>"
   */
  private extractHumanName(authorString: string): string {
    if (!authorString) {
      return 'Unknown';
    }
    
    // Handle format: "Name <email>"
    const match = authorString.match(/^([^<]+)/);
    if (match) {
      return match[1].trim();
    }
    
    return authorString;
  }

  /**
   * Extract model name from model string (e.g., "claude-3-opus-20240229" -> "Claude")
   * Returns the part before the first "-" with first letter capitalized, or null if no model.
   */
  private extractModelName(modelString: string | undefined): string | null {
    if (!modelString || modelString.trim() === '') {
      return null;
    }
    
    const trimmed = modelString.trim().toLowerCase();
    
    // Handle special cases
    if (trimmed === 'default' || trimmed === 'auto') {
      return 'Cursor';
    }
    if (trimmed === 'unknown') {
      return null; // Will display as "AI"
    }
    
    // Parse model string into parts, filtering out noise
    const parts = modelString.toLowerCase().split('-').filter(p => {
      // Skip date suffixes (8+ digits)
      if (/^\d{8,}$/.test(p)) {
        return false;
      }
      // Skip "thinking" variant
      if (p === 'thinking') {
        return false;
      }
      return true;
    });
    
    // GPT models: gpt-4o -> GPT 4o, gpt-4o-mini -> GPT 4o Mini
    if (parts[0] === 'gpt') {
      const rest = parts.slice(1);
      if (rest.length === 0) {
        return 'GPT';
      }
      // Keep version (4o, 4, 5) as-is, capitalize others (mini -> Mini, turbo -> Turbo)
      const variant = rest.map((p, i) => {
        if (i === 0) {
          return p; // 4o, 4, 5
        }
        return p.charAt(0).toUpperCase() + p.slice(1);
      }).join(' ');
      return `GPT ${variant}`;
    }
    
    // Claude models: claude-3-5-sonnet -> Sonnet 3.5, claude-opus-4 -> Opus 4
    if (parts[0] === 'claude') {
      const rest = parts.slice(1);
      
      // Find model name (opus, sonnet, haiku) and version numbers
      const modelNames = ['opus', 'sonnet', 'haiku'];
      let modelName = '';
      const versions: string[] = [];
      
      for (const p of rest) {
        if (modelNames.includes(p)) {
          modelName = p.charAt(0).toUpperCase() + p.slice(1);
        } else if (/^[\d.]+$/.test(p)) {
          versions.push(p);
        }
      }
      
      if (modelName) {
        // Combine versions with dot: 3, 5 -> 3.5
        const versionStr = versions.join('.');
        return versionStr ? `${modelName} ${versionStr}` : modelName;
      }
      
      return 'Claude';
    }
    
    // Gemini models: gemini-1.5-flash -> Gemini Flash 1.5, gemini-2.0-pro -> Gemini Pro 2.0
    if (parts[0] === 'gemini') {
      const rest = parts.slice(1);
      
      // Find variant name and version
      const variantNames = ['pro', 'flash', 'ultra', 'nano'];
      let variantName = '';
      let version = '';
      
      for (const p of rest) {
        if (variantNames.includes(p)) {
          variantName = p.charAt(0).toUpperCase() + p.slice(1);
        } else if (/^[\d.]+$/.test(p)) {
          version = p;
        }
      }
      
      if (variantName && version) {
        return `Gemini ${variantName} ${version}`;
      } else if (variantName) {
        return `Gemini ${variantName}`;
      } else if (version) {
        return `Gemini ${version}`;
      }
      return 'Gemini';
    }
    
    // o1, o3, o4-mini models: o1 -> O1, o3-mini -> O3 Mini
    if (/^o\d/.test(parts[0])) {
      return parts.map(p => {
        if (/^o\d/.test(p)) {
          return p.toUpperCase();
        }
        return p.charAt(0).toUpperCase() + p.slice(1);
      }).join(' ');
    }
    
    // Codex models: codex-5.2 -> Codex 5.2
    if (parts[0] === 'codex') {
      const version = parts.find(p => /^[\d.]+$/.test(p));
      return version ? `Codex ${version}` : 'Codex';
    }
    
    // Fallback: return the original slug as-is
    return modelString.trim();
  }

  /**
   * Build hover content showing author details.
   * Shows a polished chat-style conversation view with clear visual hierarchy.
   * Each message is shown individually with its own header and timestamp.
   */
  /**
   * Extract email from a "Name <email>" format string.
   * Returns the email if found, or null.
   */
  private extractEmail(authorString: string | null | undefined): string | null {
    if (!authorString) {
      return null;
    }
    const match = authorString.match(/<([^>]+)>/);
    return match ? match[1] : null;
  }

  private buildHoverContent(lineInfo: LineBlameInfo | undefined, documentUri?: vscode.Uri, blameResult?: BlameResult): vscode.MarkdownString {
    const md = new vscode.MarkdownString();
    md.isTrusted = true;
    md.supportHtml = true;

    if (!lineInfo || !lineInfo.isAiAuthored) {
      md.appendMarkdown('👤 **Human-authored code**\n');
      return md;
    }

    const record = lineInfo.promptRecord;
    const messages = record?.messages || [];
    const hasMessages = messages.length > 0 && messages.some(m => m.text);

    // Extract metadata for header
    const humanName = this.extractHumanName(record?.human_author || '');
    const model = record?.agent_id?.model || '';
    const tool = record?.agent_id?.tool || lineInfo.author;
    const toolCapitalized = tool.charAt(0).toUpperCase() + tool.slice(1);
    
    // Build model display: hide if default/auto/unknown/empty
    const modelLower = model.toLowerCase();
    const hideModel = !model || modelLower === 'default' || modelLower === 'auto' || modelLower === 'unknown';
    const modelDisplay = hideModel ? '' : model;

    // ═══════════════════════════════════════════════════════════════
    // COLOR BAR - Visual association with gutter highlight
    // ═══════════════════════════════════════════════════════════════
    const colorIndex = this.getColorIndexForPromptId(lineInfo.commitHash);
    const gutterColorHex = this.rgbaToHex(this.filteredColors[colorIndex] || this.HUNK_COLORS[colorIndex]);
    md.appendMarkdown(`<span style="color:${gutterColorHex};">████</span>\n\n`);

    // ═══════════════════════════════════════════════════════════════
    // TOP HEADER - Attribution with color
    // ═══════════════════════════════════════════════════════════════
    if (modelDisplay) {
      md.appendMarkdown(`<span style="color:#a78bfa;">**${humanName}**</span> with <span style="color:#60a5fa;">**${modelDisplay}**</span> in <span style="color:#f472b6;">**${toolCapitalized}**</span> · <span style="color:#94a3b8;">*powered by Git AI*</span>\n\n`);
    } else {
      md.appendMarkdown(`<span style="color:#a78bfa;">**${humanName}**</span> with <span style="color:#f472b6;">**${toolCapitalized}**</span> · <span style="color:#94a3b8;">*powered by Git AI*</span>\n\n`);
    }
    md.appendMarkdown(`---\n\n`);

    // Fallback if no messages saved - show contextual message
    if (!hasMessages) {
      // Common prefix: always mention /ask skill
      md.appendMarkdown('💡 *Ask this agent about this code with `/ask`*\n\n');

      const metadata = blameResult?.metadata;
      const hasMessagesUrl = !!record?.messages_url;

      if (hasMessagesUrl) {
        // Has messages_url but messages not loaded yet - CAS fetch in progress
        md.appendMarkdown('*Loading prompt from cloud...*\n');
      } else if (metadata?.is_logged_in) {
        // Logged in but no prompt/messages_url - prompt wasn't saved
        md.appendMarkdown('*Prompt was not saved.* Prompt Storage is enabled. Future prompts will be saved.\n');
      } else if (!metadata?.is_logged_in && metadata !== undefined) {
        // Not logged in - check if this is a teammate's code
        const currentEmail = this.extractEmail(metadata.current_user);
        const authorEmail = this.extractEmail(record?.human_author);
        const isDifferentUser = currentEmail && authorEmail && currentEmail !== authorEmail;

        if (isDifferentUser) {
          md.appendMarkdown('🔒 *Login to see prompt summaries from your teammates*\n\n');
          md.appendCodeblock('git-ai login', 'bash');
        } else {
          md.appendMarkdown('*No prompt saved.*');
        }
      } else {
        // No metadata available (backward compat) - show generic message
        md.appendMarkdown('🔒 *Transcript not saved*\n\n');
      }
      return md;
    }

    // Parse timestamps and calculate relative times
    const messagesWithTimestamps = messages.map((msg, index) => {
      let timestamp: Date | null = null;
      if (msg.timestamp) {
        timestamp = new Date(msg.timestamp);
      }
      return { ...msg, parsedTimestamp: timestamp, originalIndex: index };
    });

    // Use message 0 as the base if it has a timestamp, otherwise find the first message with a timestamp
    const baseMessage = messagesWithTimestamps[0]?.parsedTimestamp 
      ? messagesWithTimestamps[0]
      : messagesWithTimestamps.find(m => m.parsedTimestamp);
    const baseTimestamp = baseMessage?.parsedTimestamp;
    const baseIndex = baseMessage?.originalIndex ?? -1;

    // Calculate time formats for all messages
    const timeFormats = messagesWithTimestamps.map((msg, index) => {
      if (!msg.parsedTimestamp) {
        return null;
      }
      if (index === baseIndex) {
        // Base message (preferably message 0): show actual date/time
        return this.formatAbsoluteTimestamp(msg.parsedTimestamp);
      } else if (baseTimestamp) {
        // Subsequent messages: show relative time from base message
        const diffMs = msg.parsedTimestamp.getTime() - baseTimestamp.getTime();
        return this.formatRelativeTime(diffMs);
      }
      return null;
    });

    // ═══════════════════════════════════════════════════════════════
    // CONVERSATION - Show each message with its own header
    // ═══════════════════════════════════════════════════════════════
    const aiHeader = hideModel ? toolCapitalized : `${model} ${toolCapitalized}`;
    
    for (const msg of messagesWithTimestamps) {
      if (!msg.text) {
        continue;
      }
      
      const timestamp = timeFormats[msg.originalIndex];
      
      if (msg.type === 'user') {
        // User message header
        md.appendMarkdown(`#### 💬 ${humanName}`);
        if (timestamp) {
          md.appendMarkdown(` · *${timestamp}*`);
        }
        md.appendMarkdown(`\n\n`);
        md.appendMarkdown(this.formatMessageWithPadding(msg.text) + '\n\n');
      } else if (msg.type === 'assistant') {
        // Assistant message header
        md.appendMarkdown(`#### 🤖 ${aiHeader}`);
        if (timestamp) {
          md.appendMarkdown(` · *${timestamp}*`);
        }
        md.appendMarkdown(`\n\n`);
        md.appendMarkdown(this.formatMessageWithPadding(msg.text) + '\n\n');
      }
    }

    // ═══════════════════════════════════════════════════════════════
    // FOOTER
    // ═══════════════════════════════════════════════════════════════
    md.appendMarkdown(`---\n\n`);
    
    // Accepted lines count with checkmark
    const acceptedLines = record?.accepted_lines;
    if (acceptedLines !== undefined && acceptedLines > 0) {
      md.appendMarkdown(`✅ **+${acceptedLines} accepted lines**\n\n`);
    }

    // Other files section - show as clickable links
    const otherFiles = record?.other_files;
    if (otherFiles && otherFiles.length > 0) {
      md.appendMarkdown(`📁 **Other files:**\n\n`);
      
      // Get workspace folder to resolve relative paths
      let workspaceFolder: vscode.WorkspaceFolder | undefined;
      if (documentUri) {
        workspaceFolder = vscode.workspace.getWorkspaceFolder(documentUri);
      }
      
      for (const filePath of otherFiles) {
        // Construct file URI - filePath is relative to repo root
        let fileUri: vscode.Uri;
        if (workspaceFolder) {
          // Resolve relative to workspace folder
          fileUri = vscode.Uri.joinPath(workspaceFolder.uri, filePath);
        } else if (documentUri) {
          // Fallback: try to resolve relative to current document's directory
          const docDir = vscode.Uri.joinPath(documentUri, '..');
          fileUri = vscode.Uri.joinPath(docDir, filePath);
        } else {
          // Last resort: assume it's relative to workspace root
          // This might not work, but it's better than nothing
          fileUri = vscode.Uri.file(filePath);
        }
        
        // Create clickable link using command URI to open the file
        // Format: command:commandId?[encodeURIComponent(JSON.stringify([args]))]
        const commandArgs = encodeURIComponent(JSON.stringify([fileUri.toString()]));
        md.appendMarkdown(`- [${filePath}](command:vscode.open?${commandArgs})\n`);
      }
      md.appendMarkdown('\n');
    }

    // Commits section - show as clickable links to view diff
    const commits = record?.commits;
    if (commits && commits.length > 0) {
      md.appendMarkdown(`📝 **Commits:**\n\n`);
      
      // Get workspace folder to resolve git repository
      let workspaceFolder: vscode.WorkspaceFolder | undefined;
      if (documentUri) {
        workspaceFolder = vscode.workspace.getWorkspaceFolder(documentUri);
      }
      
      for (const commitSha of commits) {
        const shortSha = commitSha.substring(0, 7);
        
        if (workspaceFolder) {
          const repoPath = workspaceFolder.uri.fsPath;
          // Use our custom command to open the commit diff
          const commandArgs = encodeURIComponent(JSON.stringify([commitSha, repoPath]));
          md.appendMarkdown(`- \`${shortSha}\` [view diff](command:git-ai.showCommitDiff?${commandArgs})\n`);
        } else {
          // Fallback: just show the SHA without link if we can't resolve repo
          md.appendMarkdown(`- \`${shortSha}\`\n`);
        }
      }
      md.appendMarkdown('\n');
    }

    return md;
  }

  /**
   * Format an absolute timestamp for the first message.
   * Shows a readable date/time format.
   */
  private formatAbsoluteTimestamp(date: Date): string {
    const now = new Date();
    const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
    const messageDate = new Date(date.getFullYear(), date.getMonth(), date.getDate());
    
    // If it's today, show time only
    if (messageDate.getTime() === today.getTime()) {
      return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    }
    
    // If it's this year, show month and day
    if (date.getFullYear() === now.getFullYear()) {
      return date.toLocaleDateString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
    }
    
    // Otherwise show full date
    return date.toLocaleDateString([], { year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
  }

  /**
   * Format a relative time difference for subsequent messages.
   * Shows increments like "5 mins later", "1 hr later", etc.
   */
  private formatRelativeTime(diffMs: number): string {
    const diffSeconds = Math.floor(diffMs / 1000);
    const diffMinutes = Math.floor(diffSeconds / 60);
    const diffHours = Math.floor(diffMinutes / 60);
    const diffDays = Math.floor(diffHours / 24);
    
    if (diffDays > 0) {
      return `${diffDays} ${diffDays === 1 ? 'day' : 'days'} later`;
    } else if (diffHours > 0) {
      return `${diffHours} ${diffHours === 1 ? 'hr' : 'hrs'} later`;
    } else if (diffMinutes > 0) {
      return `${diffMinutes} ${diffMinutes === 1 ? 'min' : 'mins'} later`;
    } else if (diffSeconds > 0) {
      return `${diffSeconds} ${diffSeconds === 1 ? 'sec' : 'secs'} later`;
    } else {
      return 'just now';
    }
  }

  /**
   * Format a message for display in the hover with left padding.
   * Uses blockquotes to create a left border/indent effect.
   * Preserves markdown formatting while keeping reasonable length.
   */
  private formatMessageWithPadding(text: string): string {
    // Trim excessive whitespace but preserve structure
    let content = text.trim();
    
    // If message is very long, show first portion with indicator
    const MAX_CHARS = 2000;
    if (content.length > MAX_CHARS) {
      const truncated = content.substring(0, MAX_CHARS);
      // Try to break at a word boundary
      const lastSpace = truncated.lastIndexOf(' ');
      const breakPoint = lastSpace > MAX_CHARS - 200 ? lastSpace : MAX_CHARS;
      content = truncated.substring(0, breakPoint) + '\n\n*... message truncated ...*';
    }
    
    // Convert to blockquote for left padding effect
    // Each line gets prefixed with "> "
    return content
      .split('\n')
      .map(line => '> ' + line)
      .join('\n');
  }

  /**
   * Show commit diff by running git show and displaying in a new tab.
   */
  private async showCommitDiff(commitSha: string, workspacePath: string): Promise<void> {
    const shortSha = commitSha.substring(0, 7);
    
    try {
      const { spawn } = await import('child_process');
      
      // Run git show to get the commit diff
      const diffOutput = await new Promise<string>((resolve, reject) => {
        const proc = spawn('git', ['show', '--color=never', commitSha], {
          cwd: workspacePath
        });
        
        let stdout = '';
        let stderr = '';
        
        proc.stdout.on('data', (data) => {
          stdout += data.toString();
        });
        
        proc.stderr.on('data', (data) => {
          stderr += data.toString();
        });
        
        proc.on('error', (error) => {
          reject(error);
        });
        
        proc.on('close', (code) => {
          if (code !== 0) {
            reject(new Error(`git show failed: ${stderr}`));
          } else {
            resolve(stdout);
          }
        });
      });
      
      // Store the diff content and open as virtual document
      const contentId = `commit-${shortSha}-${Date.now()}`;
      this.markdownContentStore.set(contentId, diffOutput);
      
      // Create a virtual document URI with .diff extension for syntax highlighting
      const uri = vscode.Uri.parse(`${BlameLensManager.VIRTUAL_SCHEME}:///${contentId}.diff`);
      
      // Open the virtual document in a new tab
      const doc = await vscode.workspace.openTextDocument(uri);
      await vscode.window.showTextDocument(doc, {
        viewColumn: vscode.ViewColumn.Beside,
        preview: true
      });
      
    } catch (error) {
      console.error('[git-ai] Failed to open commit diff:', error);
      vscode.window.showErrorMessage(
        `Failed to open commit diff for ${shortSha}. ` +
        `You can view it manually with: git show ${commitSha}`
      );
    }
  }

  /**
   * Get the workspace cwd for running git-ai commands against a document.
   */
  private getWorkspaceCwd(documentUri: vscode.Uri): string | undefined {
    const repo = findRepoForFile(documentUri);
    if (repo?.rootUri) {
      return repo.rootUri.fsPath;
    }
    const workspaceFolder = vscode.workspace.getWorkspaceFolder(documentUri);
    return workspaceFolder?.uri.fsPath;
  }

  /**
   * Trigger async CAS fetches for prompts that have messages_url but no messages.
   * Updates blame result in-place and re-renders when fetches complete.
   */
  private triggerCASFetches(blameResult: BlameResult, documentUri: vscode.Uri): void {
    const cwd = this.getWorkspaceCwd(documentUri);
    if (!cwd) {
      return;
    }

    // Find prompts with messages_url but empty messages
    const promptsToFetch: Array<{ promptId: string; record: import("./blame-service").PromptRecord }> = [];
    for (const [promptId, record] of blameResult.prompts) {
      const hasMessages = record.messages && record.messages.length > 0 && record.messages.some(m => m.text);
      if (!hasMessages && record.messages_url && !this.casFetchInProgress.has(promptId)) {
        promptsToFetch.push({ promptId, record });
      }
    }

    // Cap concurrent fetches at 3
    const toFetch = promptsToFetch.slice(0, 3);

    for (const { promptId, record } of toFetch) {
      this.casFetchInProgress.add(promptId);

      this.blameService.fetchPromptFromCAS(promptId, cwd).then((messages) => {
        this.casFetchInProgress.delete(promptId);

        if (messages && this.currentBlameResult === blameResult) {
          // Update record in-place
          record.messages = messages;

          // Also update all LineBlameInfo that reference this prompt
          for (const [, lineInfo] of blameResult.lineAuthors) {
            if (lineInfo.commitHash === promptId && lineInfo.promptRecord) {
              lineInfo.promptRecord.messages = messages;
            }
          }

          // Re-render if still the active document
          const activeEditor = vscode.window.activeTextEditor;
          if (activeEditor && activeEditor.document.uri.toString() === this.currentDocumentUri) {
            this.updateStatusBar(activeEditor);
          }
        }
      }).catch(() => {
        this.casFetchInProgress.delete(promptId);
      });
    }
  }

  public dispose(): void {
    // Clear any pending document change timer
    if (this.documentChangeTimer) {
      clearTimeout(this.documentChangeTimer);
      this.documentChangeTimer = null;
    }
    
    // Clear any pending notification timeout
    if (this.notificationTimeout) {
      clearTimeout(this.notificationTimeout);
      this.notificationTimeout = null;
    }
    
    this.casFetchInProgress.clear();
    this.blameService.dispose();
    this.statusBarItem.dispose();
    this._onDidChangeVirtualDocument.dispose();
    
    // Clear markdown content store
    this.markdownContentStore.clear();
    
    // Dispose all color decorations
    this.colorDecorations.forEach(decoration => decoration.dispose());
    
    // Dispose after-text decoration
    if (this.afterTextDecoration) {
      this.afterTextDecoration.dispose();
      this.afterTextDecoration = null;
    }
  }
}

/**
 * Register the View Author command (stub for future use)
 */
export function registerBlameLensCommands(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.commands.registerCommand('git-ai.viewAuthor', (lineNumber: number) => {
      vscode.window.showInformationMessage('Hello World');
    })
  );
}
