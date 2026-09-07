import * as vscode from "vscode";
import * as fs from "fs/promises";
import * as path from "path";
import { execFile, spawn } from "child_process";
import { promisify } from "util";
import { BlameQueue } from "./blame-queue";
import { findRepoForFile, getGitRepoRoot } from "./utils/git-api";
import { getGitAiBinary, resolveGitAiBinary } from "./utils/binary-path";

const execFileAsync = promisify(execFile);

export interface BlameMetadata {
  is_logged_in: boolean;
  current_user: string | null;
}

// JSON output structure from git-ai blame --json
export interface BlameJsonOutput {
  lines: Record<string, string>;  // lineRange -> promptHash (e.g., "11-114" -> "abc1234")
  prompts: Record<string, PromptRecord>;
  metadata?: BlameMetadata;
}

export interface PromptRecord {
  agent_id: {
    tool: string;
    id: string;
    model: string;
  };
  human_author: string;
  messages?: Array<{
    type: string;
    text?: string;
    timestamp?: string;
  }>;
  total_additions?: number;
  total_deletions?: number;
  accepted_lines?: number;
  overriden_lines?: number;
  other_files?: string[];
  commits?: string[];
  messages_url?: string;
}

export interface LineBlameInfo {
  author: string;        // AI tool name (e.g., "cursor") or human indicator
  commitHash: string;    // The prompt hash for AI lines
  isAiAuthored: boolean;
  promptRecord?: PromptRecord;
}

export interface BlameResult {
  lineAuthors: Map<number, LineBlameInfo>;
  prompts: Map<string, PromptRecord>;
  metadata?: BlameMetadata;
  timestamp: number;
  totalLines: number;
}

/**
 * Service for executing git-ai blame and managing blame results.
 * Uses an LRU cache keyed by file path + content hash + HEAD commit SHA + working log state.
 */
export class BlameService {
  private static readonly TIMEOUT_MS = 30000; // 30 second timeout
  private static readonly MAX_CACHE_ENTRIES = 20;
  
  private queue: BlameQueue<BlameResult>;
  private contentCache: Map<string, BlameResult> = new Map(); // LRU cache using Map insertion order
  private gitAiAvailable: boolean | null = null;
  private hasShownInstallMessage = false;
  
  constructor() {
    this.queue = new BlameQueue<BlameResult>();
  }
  
  /**
   * Fast hash function (djb2) for content-based cache keys.
   */
  private hashContent(content: string): string {
    let hash = 5381;
    for (let i = 0; i < content.length; i++) {
      hash = ((hash << 5) + hash) ^ content.charCodeAt(i);
    }
    // Convert to unsigned 32-bit integer and then to hex string
    return (hash >>> 0).toString(16);
  }
  
  /**
   * Generate a cache key from file path, content, commit SHA, and working log state.
   */
  private getContentCacheKey(
    filePath: string,
    content: string,
    commitSha: string,
    workingLogToken: string
  ): string {
    const contentHash = this.hashContent(content);
    return `${filePath}:${commitSha}:${contentHash}:${workingLogToken}`;
  }

  private async getWorkingLogCacheToken(cwd: string | null, headCommit: string): Promise<string> {
    if (!cwd) {
      return `working-log:uncached:${Date.now()}`;
    }

    try {
      const { stdout } = await execFileAsync(
        'git',
        ['rev-parse', '--git-path', `ai/working_logs/${headCommit}`],
        { cwd, timeout: 5000 }
      );
      const workingLogPath = this.resolveGitPath(cwd, String(stdout).trim());
      if (!workingLogPath) {
        return 'working-log:missing';
      }

      return [
        'working-log',
        `checkpoints=${await this.statFileToken(path.join(workingLogPath, 'checkpoints.jsonl'))}`,
        `initial=${await this.statFileToken(path.join(workingLogPath, 'INITIAL'))}`,
        `blobs=${await this.statBlobsDirectoryToken(path.join(workingLogPath, 'blobs'))}`,
      ].join(':');
    } catch (error) {
      console.warn('[git-ai] Failed to compute working log cache token:', error);
      return `working-log:uncached:${Date.now()}`;
    }
  }

  private resolveGitPath(cwd: string, gitPath: string): string | null {
    if (!gitPath) {
      return null;
    }

    return path.isAbsolute(gitPath) ? gitPath : path.join(cwd, gitPath);
  }

  private async statFileToken(filePath: string): Promise<string> {
    try {
      const stat = await fs.stat(filePath, { bigint: true });
      return `${stat.mtimeNs}:${stat.size}`;
    } catch (error) {
      if (this.isMissingPathError(error)) {
        return 'missing';
      }
      throw error;
    }
  }

  private async statBlobsDirectoryToken(directoryPath: string): Promise<string> {
    try {
      const stat = await fs.stat(directoryPath, { bigint: true });
      return `dir:${stat.mtimeNs}`;
    } catch (error) {
      if (this.isMissingPathError(error)) {
        return 'missing';
      }
      throw error;
    }
  }

  private isMissingPathError(error: unknown): boolean {
    return error instanceof Error && 'code' in error && error.code === 'ENOENT';
  }
  
  /**
   * Add an entry to the LRU cache, evicting the oldest entry if necessary.
   */
  private addToCache(key: string, result: BlameResult): void {
    // If key exists, delete it first so it gets re-added at the end (LRU refresh)
    if (this.contentCache.has(key)) {
      this.contentCache.delete(key);
    }
    
    // Evict oldest entry if cache is full
    if (this.contentCache.size >= BlameService.MAX_CACHE_ENTRIES) {
      const oldestKey = this.contentCache.keys().next().value;
      if (oldestKey) {
        this.contentCache.delete(oldestKey);
      }
    }
    
    this.contentCache.set(key, result);
  }
  
  /**
   * Get an entry from the cache, refreshing its position in the LRU order.
   */
  private getFromCache(key: string): BlameResult | undefined {
    const result = this.contentCache.get(key);
    if (result) {
      // Refresh LRU position by deleting and re-adding
      this.contentCache.delete(key);
      this.contentCache.set(key, result);
    }
    return result;
  }
  
  /**
   * Request blame information for a document.
   * Returns null if git-ai is not available, file is not in git, or an error occurs.
   */
  public async requestBlame(
    document: vscode.TextDocument,
    priority: 'high' | 'normal' = 'normal'
  ): Promise<BlameResult | null> {
    // Only process file:// URIs
    if (document.uri.scheme !== 'file') {
      return null;
    }
    
    // Check if git-ai is available
    if (this.gitAiAvailable === false) {
      return null;
    }
    
    // Look up repo once to get both HEAD commit (for cache key) and repo root (for cwd)
    const repo = findRepoForFile(document.uri);
    const headCommit = repo?.state.HEAD?.commit ?? null;
    const gitRepoRoot = repo?.rootUri.fsPath ?? null;

    if (!headCommit) {
      // No git repo found, fall back to executing without cache
      return this.queue.enqueue(
        document.uri,
        priority,
        (signal) => this.executeBlame(document, document.getText(), signal, gitRepoRoot)
      );
    }

    // Get current content for cache key and to pass to git-ai
    const content = document.getText();
    const workingLogToken = await this.getWorkingLogCacheToken(gitRepoRoot, headCommit);
    const cacheKey = this.getContentCacheKey(document.uri.fsPath, content, headCommit, workingLogToken);

    // Check LRU cache first
    const cached = this.getFromCache(cacheKey);
    if (cached) {
      return cached;
    }

    // Enqueue the blame request
    const result = await this.queue.enqueue(
      document.uri,
      priority,
      (signal) => this.executeBlame(document, content, signal, gitRepoRoot)
    );
    
    // Cache the result
    if (result) {
      this.addToCache(cacheKey, result);
    }
    
    return result;
  }
  
  /**
   * Cancel any pending blame for the given URI.
   * Called when a tab is closed.
   */
  public cancelForUri(uri: vscode.Uri): void {
    this.queue.cancelForUri(uri);
  }
  
  /**
   * Invalidate cache entries for a document.
   * Called when a file is saved. Removes all cache entries for the given file path.
   */
  public invalidateCache(uri: vscode.Uri): void {
    const filePath = uri.fsPath;
    // Remove all cache entries for this file (they start with the file path)
    for (const key of this.contentCache.keys()) {
      if (key.startsWith(filePath + ':')) {
        this.contentCache.delete(key);
      }
    }
  }
  
  /**
   * Clear all cached blame data.
   */
  public clearCache(): void {
    this.contentCache.clear();
  }
  
  /**
   * Cancel all pending operations and clear cache.
   */
  public dispose(): void {
    this.queue.cancelAll();
    this.contentCache.clear();
  }
  
  private async executeBlame(
    document: vscode.TextDocument,
    content: string,
    signal: AbortSignal,
    gitRepoRoot: string | null = null
  ): Promise<BlameResult> {
    const filePath = document.uri.fsPath;
    // Use git repository root as cwd, fallback to workspace folder if git repo not found
    gitRepoRoot = gitRepoRoot ?? getGitRepoRoot(document.uri);
    const workspaceFolder = vscode.workspace.getWorkspaceFolder(document.uri);
    const cwd = gitRepoRoot || workspaceFolder?.uri.fsPath;

    // Ensure binary path is resolved before spawning
    await resolveGitAiBinary();

    return new Promise((resolve, reject) => {
      if (signal.aborted) {
        reject(new Error('Aborted'));
        return;
      }

      // Use --contents - to read file contents from stdin
      // This allows git-ai to properly shift AI attributions for dirty files
      const args = ['blame', '--json', '--include-uncommitted', '--contents', '-', filePath];
      const binary = getGitAiBinary();
      console.log('[git-ai] Spawning blame:', { binary, args, cwd });
      const proc = spawn(binary, args, {
        cwd,
        timeout: BlameService.TIMEOUT_MS,
      });
      
      let stdout = '';
      let stderr = '';
      
      // Handle abort signal
      const abortHandler = () => {
        proc.kill('SIGTERM');
        reject(new Error('Aborted'));
      };
      signal.addEventListener('abort', abortHandler);
      
      // Write file contents to stdin and close it
      proc.stdin.write(content);
      proc.stdin.end();
      
      proc.stdout.on('data', (data) => {
        stdout += data.toString();
      });
      
      proc.stderr.on('data', (data) => {
        stderr += data.toString();
      });
      
      proc.on('error', (error: NodeJS.ErrnoException) => {
        signal.removeEventListener('abort', abortHandler);
        
        if (error.code === 'ENOENT') {
          // git-ai not installed
          this.gitAiAvailable = false;
          this.showInstallMessage();
          reject(new Error('git-ai not installed'));
        } else {
          reject(error);
        }
      });
      
      proc.on('close', (code) => {
        signal.removeEventListener('abort', abortHandler);
        
        if (signal.aborted) {
          reject(new Error('Aborted'));
          return;
        }
        
        if (code !== 0) {
          // Check for common error cases
          if (stderr.includes('not a git repository')) {
            reject(new Error('Not a git repository'));
          } else if (stderr.includes('no such path') || stderr.includes('does not exist')) {
            reject(new Error('File not tracked in git'));
          } else {
            console.error('[git-ai] blame error:', stderr);
            reject(new Error(`git-ai blame failed with code ${code}`));
          }
          return;
        }
        
        // Mark git-ai as available
        this.gitAiAvailable = true;
        
        try {
          console.log('[git-ai] Raw blame stdout (first 500 chars):', stdout.substring(0, 500));
          console.log('[git-ai] Raw blame stderr:', stderr);
          const jsonOutput = JSON.parse(stdout) as BlameJsonOutput;
          const result = this.parseBlameOutput(jsonOutput, document.lineCount);
          resolve(result);
        } catch (parseError) {
          console.error('[git-ai] Failed to parse blame JSON:', parseError);
          reject(new Error('Failed to parse blame output'));
        }
      });
    });
  }
  
  /**
   * Parse the JSON output from git-ai blame and expand line ranges.
   */
  private parseBlameOutput(output: BlameJsonOutput, totalLines: number): BlameResult {
    const lineAuthors = new Map<number, LineBlameInfo>();
    const prompts = new Map<string, PromptRecord>();
    
    // Copy prompts to our map
    for (const [hash, record] of Object.entries(output.prompts || {})) {
      const msgs = record.messages || [];
      console.log(`[git-ai] parseBlameOutput prompt ${hash}: messages=${msgs.length}, hasText=${msgs.some(m => m.text)}, messages_url=${record.messages_url || 'none'}`);
      prompts.set(hash, record);
    }
    
    // Expand line ranges and map to authors
    for (const [rangeKey, promptHash] of Object.entries(output.lines || {})) {
      const lines = this.expandRangeKey(rangeKey);
      const promptRecord = prompts.get(promptHash);
      
      for (const lineNum of lines) {
        lineAuthors.set(lineNum, {
          author: promptRecord?.agent_id?.tool || 'AI',
          commitHash: promptHash,
          isAiAuthored: true,
          promptRecord,
        });
      }
    }
    
    return {
      lineAuthors,
      prompts,
      metadata: output.metadata,
      timestamp: Date.now(),
      totalLines,
    };
  }
  
  /**
   * Expand a range key like "11-114" to an array of line numbers.
   * Single line keys like "133" return [133].
   * Range keys use inclusive intervals: "11-114" means lines 11 through 114.
   */
  private expandRangeKey(rangeKey: string): number[] {
    const result: number[] = [];
    
    if (rangeKey.includes('-')) {
      const [startStr, endStr] = rangeKey.split('-');
      const start = parseInt(startStr, 10);
      const end = parseInt(endStr, 10);
      
      if (!isNaN(start) && !isNaN(end)) {
        // Inclusive range: [start, end]
        for (let line = start; line <= end; line++) {
          result.push(line);
        }
      }
    } else {
      const lineNum = parseInt(rangeKey, 10);
      if (!isNaN(lineNum)) {
        result.push(lineNum);
      }
    }
    
    return result;
  }
  
  /**
   * Fetch prompt messages from CAS via `git-ai show-prompt`.
   * Returns the messages array on success, or null on failure/timeout.
   */
  public async fetchPromptFromCAS(
    promptId: string,
    cwd: string
  ): Promise<Array<{ type: string; text?: string; timestamp?: string }> | null> {
    await resolveGitAiBinary();
    return new Promise((resolve) => {
      const args = ['show-prompt', promptId];
      const proc = spawn(getGitAiBinary(), args, {
        cwd,
        timeout: 15000,
      });

      let stdout = '';

      proc.stdout.on('data', (data) => {
        stdout += data.toString();
      });

      proc.stderr.on('data', () => {}); // Ignore stderr

      proc.on('error', () => {
        resolve(null);
      });

      proc.on('close', (code) => {
        if (code !== 0) {
          resolve(null);
          return;
        }

        try {
          const parsed = JSON.parse(stdout);
          const messages = parsed?.prompt?.messages;
          if (Array.isArray(messages) && messages.length > 0) {
            resolve(messages);
          } else {
            resolve(null);
          }
        } catch {
          resolve(null);
        }
      });
    });
  }

  private showInstallMessage(): void {
    if (this.hasShownInstallMessage) {
      return;
    }
    this.hasShownInstallMessage = true;
    
    vscode.window.showInformationMessage(
      'git-ai is not installed. Install it to see AI authorship information.',
      'Learn More'
    ).then((choice) => {
      if (choice === 'Learn More') {
        vscode.env.openExternal(
          vscode.Uri.parse('https://github.com/git-ai-project/git-ai')
        );
      }
    });
  }
  
  /**
   * Get the author display text for a line.
   * Returns the AI tool name if AI-authored, or undefined if human-authored.
   */
  public static getAuthorDisplay(lineInfo: LineBlameInfo | undefined): string | undefined {
    if (!lineInfo) {
      return undefined; // Human authored (not in blame data)
    }
    
    if (lineInfo.isAiAuthored) {
      // Capitalize the tool name
      const tool = lineInfo.author;
      return tool.charAt(0).toUpperCase() + tool.slice(1);
    }
    
    return undefined;
  }
}


