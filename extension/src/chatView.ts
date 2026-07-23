import * as os from "os";
import * as vscode from "vscode";
import { DaemonClient, discover } from "./daemon";

type FileAtRegistrar = (
  fn: (session: string, hash: string, path: string) => Promise<string>
) => void;

export class ChatViewProvider implements vscode.WebviewViewProvider {
  private view?: vscode.WebviewView;
  private client?: DaemonClient;
  private session?: string;
  private disposeStream?: () => void;
  private pendingDelta = "";

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly registerFileAt?: FileAtRegistrar
  ) {}

  resolveWebviewView(
    webviewView: vscode.WebviewView,
    _context: vscode.WebviewViewResolveContext,
    _token: vscode.CancellationToken
  ): void {
    this.view = webviewView;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [],
    };

    const info = discover();
    if (!info) {
      webviewView.webview.html = this.emptyHtml();
      return;
    }

    this.client = new DaemonClient(info);
    webviewView.webview.html = this.chatHtml();

    webviewView.webview.onDidReceiveMessage((msg: {
      type?: string;
      text?: string;
      attachments?: { name: string; content: string }[];
      requestId?: string;
      approved?: boolean;
      path?: string;
      line?: number;
      session?: string;
      hash?: string;
      action?: string;
      childId?: string;
      limitUsd?: number;
    }) => {
      switch (msg.type) {
        case "send":
          if (typeof msg.text === "string") {
            void this.handleSend(msg.text, msg.attachments);
          }
          break;
        case "approve":
          if (
            typeof msg.requestId === "string" &&
            typeof msg.approved === "boolean"
          ) {
            void this.handleApprove(msg.requestId, msg.approved);
          }
          break;
        case "control":
          if (typeof msg.action === "string") {
            void this.handleControl(msg.action);
          }
          break;
        case "openFile":
          if (typeof msg.path === "string") {
            void this.handleOpenFile(msg.path, msg.line);
          }
          break;
        case "openDiff":
          if (
            typeof msg.session === "string" &&
            typeof msg.hash === "string" &&
            typeof msg.path === "string"
          ) {
            void this.handleOpenDiff(msg.session, msg.hash, msg.path);
          }
          break;
        case "viewCheckpointDiff":
          if (typeof msg.hash === "string") {
            void this.handleViewCheckpointDiff(msg.hash);
          }
          break;
        case "revertCheckpoint":
          if (typeof msg.hash === "string") {
            void this.handleRevertCheckpoint(msg.hash);
          }
          break;
        case "raiseBudget":
          if (typeof msg.limitUsd === "number") {
            void this.handleRaiseBudget(msg.limitUsd);
          }
          break;
        case "adoptWork":
          if (typeof msg.childId === "string") {
            void this.handleAdoptWork(msg.childId);
          }
          break;
        case "pickFile":
          void this.handlePickFile(webviewView.webview);
          break;
      }
    });

    webviewView.onDidDispose(() => {
      this.disposeStream?.();
      this.disposeStream = undefined;
      this.view = undefined;
    });

    void this.initSession(webviewView.webview);
  }

  async openSession(id: string): Promise<void> {
    if (!this.client || !this.view) {
      return;
    }
    this.session = id;
    try {
      await this.loadSession();
    } catch (err) {
      this.view.webview.postMessage({
        type: "error",
        text: err instanceof Error ? err.message : "failed to open session",
      });
    }
  }

  getSession(): string | undefined {
    return this.session;
  }

  getClient(): DaemonClient | undefined {
    return this.client;
  }

  private async initSession(webview: vscode.Webview): Promise<void> {
    if (!this.client) {
      return;
    }

    try {
      const workspaceFolder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      const sessions = await this.client.listSessions();
      this.session =
        sessions.length > 0
          ? sessions[0]!
          : await this.client.createSession(workspaceFolder);

      this.registerFileAt?.((session, hash, path) =>
        this.client!.fileAt(session, hash, path)
      );

      await this.loadSession(webview);
    } catch (err) {
      webview.postMessage({
        type: "error",
        text: err instanceof Error ? err.message : "failed to connect",
      });
    }
  }

  private async loadSession(webview?: vscode.Webview): Promise<void> {
    const wv = webview ?? this.view?.webview;
    if (!this.client || !this.session || !wv) {
      return;
    }

    this.pendingDelta = "";
    this.disposeStream?.();

    const [events, details] = await Promise.all([
      this.client.events(this.session, 0),
      this.client.sessionsDetail(),
    ]);
    const detail = details.find((d) => d.id === this.session);

    wv.postMessage({ type: "reset" });
    wv.postMessage({
      type: "init",
      session: this.session,
      kind: detail?.kind ?? "orchestrator",
      events,
    });

    this.disposeStream = this.client.stream(
      this.session,
      (e) => this.onLedger(e),
      (t) => this.onDelta(t)
    );
  }

  private async handleControl(action: string): Promise<void> {
    if (!this.client || !this.session || !this.view) {
      return;
    }

    try {
      switch (action) {
        case "pause":
          await this.client.pause(this.session);
          break;
        case "resume":
          await this.client.resume(this.session);
          break;
        case "steer": {
          const text = await vscode.window.showInputBox({
            prompt: "Steer text",
          });
          if (!text) {
            return;
          }
          await this.client.steer(this.session, text);
          break;
        }
        case "interrupt":
          await this.client.interrupt(this.session);
          break;
        default:
          return;
      }
    } catch (err) {
      this.view.webview.postMessage({
        type: "error",
        text: err instanceof Error ? err.message : "control failed",
      });
    }
  }

  private async handleSend(
    text: string,
    attachments?: { name: string; content: string }[]
  ): Promise<void> {
    if (!this.client || !this.session || !this.view) {
      return;
    }

    const trimmed = text.trim();
    if (!trimmed) {
      return;
    }

    this.pendingDelta = "";
    this.view.webview.postMessage({
      type: "userMessage",
      text: trimmed,
      attachments,
    });

    try {
      await this.client.sendMessage(this.session, trimmed, attachments);
    } catch (err) {
      this.view.webview.postMessage({
        type: "error",
        text: err instanceof Error ? err.message : "send failed",
      });
    }
  }

  private async handleApprove(
    requestId: string,
    approved: boolean
  ): Promise<void> {
    if (!this.client || !this.session || !this.view) {
      return;
    }

    try {
      await this.client.approve(this.session, requestId, approved);
    } catch (err) {
      this.view.webview.postMessage({
        type: "error",
        text: err instanceof Error ? err.message : "approve failed",
      });
    }
  }

  private async handleOpenFile(path: string, line?: number): Promise<void> {
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) {
      return;
    }

    let filePath = path;
    if (path.startsWith("~")) {
      filePath = path.replace(/^~/, os.homedir());
    } else if (!path.startsWith("/")) {
      filePath = vscode.Uri.joinPath(folder.uri, path).fsPath;
    }

    const uri = vscode.Uri.file(filePath);
    try {
      await vscode.workspace.fs.stat(uri);
    } catch {
      return;
    }

    const doc = await vscode.window.showTextDocument(uri);
    if (line !== undefined && line > 0) {
      const pos = new vscode.Position(line - 1, 0);
      doc.selection = new vscode.Selection(pos, pos);
      doc.revealRange(new vscode.Range(pos, pos));
    }
  }

  private async handleOpenDiff(
    session: string,
    hash: string,
    relPath: string
  ): Promise<void> {
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) {
      return;
    }

    const encodedPath = relPath
      .split("/")
      .map((seg) => encodeURIComponent(seg))
      .join("/");
    const shadowUri = vscode.Uri.parse(
      `forge-shadow:/${session}/${hash}/${encodedPath}`
    );
    const fileUri = vscode.Uri.joinPath(folder.uri, relPath);
    await vscode.commands.executeCommand(
      "vscode.diff",
      shadowUri,
      fileUri,
      `${relPath} (checkpoint ↔ now)`
    );
  }

  private async handlePickFile(webview: vscode.Webview): Promise<void> {
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) {
      return;
    }

    const maxBytes = 256 * 1024;
    const files = await vscode.workspace.findFiles(
      "**/*",
      "**/{node_modules,target,.git}/**",
      500
    );
    const items = files.map((uri) => {
      const rel = vscode.workspace.asRelativePath(uri);
      return { label: rel, uri, name: rel };
    });
    items.sort((a, b) => a.label.localeCompare(b.label));

    const picked = await vscode.window.showQuickPick(items, {
      placeHolder: "Attach file",
    });
    if (!picked) {
      return;
    }

    try {
      const data = await vscode.workspace.fs.readFile(picked.uri);
      if (data.byteLength > maxBytes) {
        void vscode.window.showErrorMessage(
          `File exceeds 256 KiB limit (${picked.name})`
        );
        return;
      }
      const content = Buffer.from(data).toString("utf8");
      webview.postMessage({
        type: "filePicked",
        name: picked.name,
        content,
      });
    } catch (err) {
      void vscode.window.showErrorMessage(
        err instanceof Error ? err.message : "failed to read file"
      );
    }
  }

  private async handleViewCheckpointDiff(hash: string): Promise<void> {
    if (!this.client || !this.session) {
      return;
    }

    try {
      const { patch } = await this.client.diff(this.session, hash);
      const shortHash = hash.slice(0, 7);
      const uri = vscode.Uri.parse(
        `untitled:checkpoint ${shortHash} → now`
      );
      const edit = new vscode.WorkspaceEdit();
      edit.createFile(uri, { ignoreIfExists: true, overwrite: true });
      edit.insert(uri, new vscode.Position(0, 0), patch);
      await vscode.workspace.applyEdit(edit);
      const doc = await vscode.workspace.openTextDocument(uri);
      await vscode.languages.setTextDocumentLanguage(doc, "diff");
      await vscode.window.showTextDocument(doc, { preview: false });
    } catch (err) {
      void vscode.window.showErrorMessage(
        err instanceof Error ? err.message : "diff failed"
      );
    }
  }

  private async handleRevertCheckpoint(hash: string): Promise<void> {
    if (!this.client || !this.session) {
      return;
    }

    const shortHash = hash.slice(0, 7);
    const confirmed = await vscode.window.showWarningMessage(
      `Revert workspace to checkpoint ${shortHash}?`,
      { modal: true },
      "Revert"
    );
    if (confirmed !== "Revert") {
      return;
    }

    try {
      await this.client.restore(this.session, hash);
    } catch (err) {
      void vscode.window.showErrorMessage(
        err instanceof Error ? err.message : "restore failed"
      );
    }
  }

  private async handleRaiseBudget(currentLimit: number): Promise<void> {
    if (!this.client || !this.session) {
      return;
    }

    const prefilled = (currentLimit * 2).toString();
    const input = await vscode.window.showInputBox({
      prompt: "New session budget (USD)",
      value: prefilled,
    });
    if (!input) {
      return;
    }

    const usd = parseFloat(input);
    if (!Number.isFinite(usd) || usd <= 0) {
      void vscode.window.showErrorMessage("Budget must be a positive number.");
      return;
    }

    try {
      await this.client.raiseBudget(this.session, usd);
    } catch (err) {
      void vscode.window.showErrorMessage(
        err instanceof Error ? err.message : "budget raise failed"
      );
    }
  }

  private async handleAdoptWork(childId: string): Promise<void> {
    if (!this.client || !this.session) {
      return;
    }

    const branch = `fc/${childId.toLowerCase()}`;
    const confirmed = await vscode.window.showWarningMessage(
      `Merge ${branch} into your branch and remove the worktree?`,
      { modal: true },
      "Adopt work"
    );
    if (confirmed !== "Adopt work") {
      return;
    }

    try {
      await this.client.adopt(this.session, childId);
    } catch (err) {
      void vscode.window.showErrorMessage(
        err instanceof Error ? err.message : "adopt failed"
      );
    }
  }

  private onLedger(e: {
    kind?: string;
    actor?: string;
    body?: Record<string, unknown>;
  }): void {
    if (!this.view) {
      return;
    }

    if (e.kind === "message" && e.actor === "orchestrator") {
      this.pendingDelta = "";
      this.view.webview.postMessage({
        type: "assistantMessage",
        text: (e.body?.text as string) ?? "",
      });
      return;
    }

    if (
      e.kind === "tool_call" ||
      e.kind === "tool_result" ||
      e.kind === "approval_request" ||
      e.kind === "approval_decision" ||
      e.kind === "usage" ||
      e.kind === "dispatch" ||
      e.kind === "steer" ||
      e.kind === "context_inject" ||
      e.kind === "pause" ||
      e.kind === "resume" ||
      e.kind === "interrupt" ||
      e.kind === "budget" ||
      e.kind === "adopt" ||
      e.kind === "verdict" ||
      e.kind === "error" ||
      (e.kind === "message" && e.actor === "human") ||
      (e.kind === "message" &&
        typeof e.actor === "string" &&
        e.actor.startsWith("sub:"))
    ) {
      this.view.webview.postMessage({ type: "event", event: e });
    }
  }

  private onDelta(t: string): void {
    if (!this.view) {
      return;
    }
    this.pendingDelta += t;
    this.view.webview.postMessage({ type: "delta", text: this.pendingDelta });
  }

  private emptyHtml(): string {
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline';">
<style>
  body {
    font-family: var(--vscode-font-family);
    color: var(--vscode-foreground);
    padding: 1rem;
  }
  .empty {
    text-align: center;
    margin-top: 2rem;
    opacity: 0.85;
    line-height: 1.6;
  }
  code {
    background: var(--vscode-textCodeBlock-background);
    padding: 0.2em 0.4em;
    border-radius: 3px;
  }
</style>
</head>
<body>
  <div class="empty">
    <p>composerd is not running</p>
    <p>Start it with: <code>composerd serve</code></p>
  </div>
</body>
</html>`;
  }

  private chatHtml(): string {
    const nonce = getNonce();
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';">
<style>
  * { box-sizing: border-box; }
  html, body { height: 100%; margin: 0; }
  body {
    font-family: var(--vscode-font-family);
    color: var(--vscode-foreground);
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  #messages {
    flex: 1;
    overflow-y: auto;
    padding: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .msg {
    padding: 0.5rem 0.75rem;
    border-radius: 6px;
    max-width: 90%;
    word-wrap: break-word;
    white-space: pre-wrap;
  }
  .msg.user .attach-chips {
    margin-top: 0.35rem;
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem;
    font-size: 0.85em;
    opacity: 0.9;
  }
  .msg.user .attach-chip-inline {
    background: var(--vscode-badge-background);
    color: var(--vscode-badge-foreground);
    padding: 0.1rem 0.4rem;
    border-radius: 999px;
  }
  .msg.assistant {
    align-self: stretch;
    max-width: 100%;
    background: transparent;
    border-left: 2px solid var(--vscode-panel-border);
    border-radius: 0;
    padding-left: 0.6rem;
  }
  .msg.pending { opacity: 0.75; }
  .msg a.file-link {
    color: var(--vscode-textLink-foreground);
    text-decoration: underline;
    cursor: pointer;
  }
  .event-card {
    align-self: stretch;
    padding: 0.5rem 0.75rem;
    border-radius: 6px;
    border: 1px solid var(--vscode-panel-border);
    background: var(--vscode-editor-background);
    font-size: 0.9em;
  }
  .event-card.tool-call {
    background: var(--vscode-editor-inactiveSelectionBackground);
  }
  .event-card.tool-result details summary {
    cursor: pointer;
    user-select: none;
  }
  .event-card.tool-result.denied {
    border-color: var(--vscode-inputValidation-errorBorder);
    color: var(--vscode-errorForeground);
  }
  .event-card.tool-result .output {
    margin: 0.5rem 0 0;
    white-space: pre-wrap;
    word-wrap: break-word;
    font-family: var(--vscode-editor-font-family);
    font-size: 0.85em;
    max-height: 12rem;
    overflow-y: auto;
  }
  .event-card.tool-result .output a.file-link {
    color: var(--vscode-textLink-foreground);
  }
  .event-card.tool-result .diff-row { margin-top: 0.5rem; }
  .event-card.tool-result .btn-diff {
    padding: 0.2rem 0.6rem;
    background: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-family: inherit;
    font-size: 0.85em;
  }
  .event-card.approval .summary { margin-bottom: 0.5rem; }
  .event-card.approval .actions { display: flex; gap: 0.5rem; }
  .event-card.approval button {
    padding: 0.25rem 0.75rem;
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-family: inherit;
    font-size: 0.85em;
  }
  .event-card.approval .btn-approve {
    background: var(--vscode-button-background);
    color: var(--vscode-button-foreground);
  }
  .event-card.approval .btn-deny {
    background: var(--vscode-input-background);
    color: var(--vscode-foreground);
    border: 1px solid var(--vscode-panel-border);
  }
  .event-card.approval .decision { font-weight: 500; }
  .event-card.approval .decision.approved { color: var(--vscode-testing-iconPassed); }
  .event-card.approval .decision.denied { color: var(--vscode-errorForeground); }
  .event-muted {
    align-self: stretch;
    opacity: 0.65;
    font-size: 0.8em;
    padding: 0.15rem 0.5rem;
  }
  .event-card.budget .actions { margin-top: 0.5rem; }
  .event-card.budget button,
  .event-card.report .btn-adopt,
  .event-card.checkpoint .btn-action {
    padding: 0.2rem 0.6rem;
    background: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-family: inherit;
    font-size: 0.85em;
    margin-right: 0.35rem;
  }
  .msg.user {
    align-self: flex-end;
    background: var(--vscode-input-background);
  }
  .event-card.budget {
    border-color: var(--vscode-inputValidation-errorBorder);
    color: var(--vscode-errorForeground);
  }
  .event-card.verdict.pass {
    border-left: 3px solid var(--vscode-testing-iconPassed);
    color: var(--vscode-testing-iconPassed);
  }
  .event-card.verdict.fail {
    border-left: 3px solid var(--vscode-errorForeground);
    color: var(--vscode-errorForeground);
  }
  .event-card.verdict .journal-path {
    margin-top: 0.35rem;
    font-family: var(--vscode-editor-font-family);
    font-size: 0.85em;
    opacity: 0.65;
    color: var(--vscode-foreground);
    word-break: break-all;
  }
  .event-card.report {
    border-left: 3px solid var(--vscode-textLink-foreground);
  }
  .event-card.report .report-label {
    font-size: 0.8em;
    opacity: 0.75;
    margin-bottom: 0.25rem;
  }
  #control-strip {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.35rem 0.5rem;
    padding: 0.4rem 0.75rem;
    border-bottom: 1px solid var(--vscode-panel-border);
    font-size: 0.85em;
  }
  #session-label {
    font-family: var(--vscode-editor-font-family);
    opacity: 0.85;
  }
  #kind-badge {
    padding: 0.1rem 0.45rem;
    border-radius: 999px;
    background: var(--vscode-badge-background);
    color: var(--vscode-badge-foreground);
    font-size: 0.8em;
  }
  #control-strip button {
    padding: 0.2rem 0.55rem;
    background: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-family: inherit;
    font-size: 0.85em;
  }
  #control-strip button:hover {
    background: var(--vscode-button-secondaryHoverBackground);
  }
  #attachments {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    padding: 0.35rem 0.5rem 0;
    min-height: 0;
  }
  .attach-chip {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    padding: 0.15rem 0.5rem;
    border-radius: 999px;
    background: var(--vscode-badge-background);
    color: var(--vscode-badge-foreground);
    font-size: 0.8em;
    cursor: pointer;
  }
  .attach-chip .remove { opacity: 0.75; }
  #input-area {
    display: flex;
    gap: 0.5rem;
    padding: 0.5rem;
    border-top: 1px solid var(--vscode-panel-border);
    align-items: flex-end;
  }
  #attach {
    padding: 0.5rem 0.65rem;
    background: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-family: inherit;
    font-weight: 600;
  }
  #input {
    flex: 1;
    padding: 0.5rem;
    background: var(--vscode-input-background);
    color: var(--vscode-input-foreground);
    border: 1px solid var(--vscode-input-border);
    border-radius: 4px;
    font-family: inherit;
    resize: none;
  }
  #send {
    padding: 0.5rem 1rem;
    background: var(--vscode-button-background);
    color: var(--vscode-button-foreground);
    border: none;
    border-radius: 4px;
    cursor: pointer;
  }
  #send:hover { background: var(--vscode-button-hoverBackground); }
</style>
</head>
<body>
  <div id="control-strip">
    <span id="session-label"></span>
    <span id="kind-badge"></span>
    <button id="btn-pause" type="button">Pause ⏸</button>
    <button id="btn-resume" type="button">Resume ▶</button>
    <button id="btn-steer" type="button">Steer</button>
    <button id="btn-stop" type="button">Stop ■</button>
  </div>
  <div id="attachments"></div>
  <div id="messages"></div>
  <div id="input-area">
    <button id="attach" title="Attach file">📎</button>
    <textarea id="input" rows="2" placeholder="Message Forge Composer…"></textarea>
    <button id="send">Send</button>
  </div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const messagesEl = document.getElementById("messages");
    const attachmentsEl = document.getElementById("attachments");
    const sessionLabelEl = document.getElementById("session-label");
    const kindBadgeEl = document.getElementById("kind-badge");
    const inputEl = document.getElementById("input");
    const sendBtn = document.getElementById("send");
    const attachBtn = document.getElementById("attach");
    let pendingEl = null;
    let sessionId = "";
    let attachments = [];
    const toolCallArgs = {};
    const FILE_LINK_RE = /(~?[\\w./+-]*\\/)?[\\w.+-]+\\.[A-Za-z0-9]{1,8}(:\\d+)?/g;

    function updateSessionHeader(id, kind) {
      sessionId = id || "";
      if (sessionLabelEl) {
        sessionLabelEl.textContent = id ? id.slice(-8) : "";
      }
      if (kindBadgeEl) {
        kindBadgeEl.textContent = kind || "orchestrator";
      }
    }

    function escapeHtml(s) {
      return s
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");
    }

    function linkify(text) {
      const escaped = escapeHtml(text);
      return escaped.replace(FILE_LINK_RE, (match) => {
        let filePath = match;
        let line;
        const lineMatch = match.match(/:(\\d+)$/);
        if (lineMatch) {
          line = parseInt(lineMatch[1], 10);
          filePath = match.slice(0, -lineMatch[0].length);
        }
        const lineAttr = line ? ' data-line="' + line + '"' : "";
        return (
          '<a href="#" class="file-link" data-path="' +
          escapeHtml(filePath) +
          '"' +
          lineAttr +
          ">" +
          escapeHtml(match) +
          "</a>"
        );
      });
    }

    function bindFileLinks(container) {
      container.querySelectorAll("a.file-link").forEach((a) => {
        a.addEventListener("click", (ev) => {
          ev.preventDefault();
          const path = a.getAttribute("data-path") || "";
          const lineStr = a.getAttribute("data-line");
          const line = lineStr ? parseInt(lineStr, 10) : undefined;
          vscode.postMessage({ type: "openFile", path, line });
        });
      });
    }

    function renderAttachments() {
      attachmentsEl.innerHTML = "";
      for (const att of attachments) {
        const chip = document.createElement("span");
        chip.className = "attach-chip";
        chip.title = "Remove attachment";
        chip.innerHTML =
          "📎 " + escapeHtml(att.name) + ' <span class="remove">×</span>';
        chip.addEventListener("click", () => {
          attachments = attachments.filter((a) => a.name !== att.name);
          renderAttachments();
        });
        attachmentsEl.appendChild(chip);
      }
    }

    function toolCallSummary(body) {
      const name = body.name || "tool";
      const args = body.arguments;
      let summary = "";
      if (args && typeof args === "object") {
        const parts = [];
        for (const [k, v] of Object.entries(args)) {
          let sv = typeof v === "string" ? v : JSON.stringify(v);
          if (sv.length > 60) sv = sv.slice(0, 57) + "...";
          parts.push(k + "=" + sv);
        }
        summary = parts.join(", ");
      } else if (args) {
        summary = String(args);
      }
      return "⚙ " + name + (summary ? " " + summary : "");
    }

    function toolResultSummary(body) {
      const name = body.name || "tool";
      const denied = body.denied === true;
      const ok = body.ok === true;
      const prefix = denied ? "⛔ DENIED " : ok ? "✓ " : "✗ ";
      return prefix + name;
    }

    function renderEvent(e) {
      const kind = e.kind;
      const body = e.body || {};

      if (kind === "message" && e.actor === "human") {
        addMessage("user", body.text || "", false, body.attachments);
        return;
      }

      if (kind === "tool_call") {
        if (body.id) {
          toolCallArgs[body.id] = body.arguments;
        }
        const el = document.createElement("div");
        el.className = "event-card tool-call";
        el.innerHTML = linkify(toolCallSummary(body));
        bindFileLinks(el);
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "tool_result") {
        const el = document.createElement("div");
        const denied = body.denied === true;
        el.className = "event-card tool-result" + (denied ? " denied" : "");
        const details = document.createElement("details");
        const summary = document.createElement("summary");
        summary.textContent = toolResultSummary(body);
        details.appendChild(summary);
        const output = document.createElement("div");
        output.className = "output";
        output.innerHTML = linkify(body.output || "");
        bindFileLinks(output);
        details.appendChild(output);
        el.appendChild(details);

        if (body.checkpoint) {
          const diffRow = document.createElement("div");
          diffRow.className = "diff-row";
          const viewBtn = document.createElement("button");
          viewBtn.className = "btn-diff";
          viewBtn.textContent = "View diff";
          viewBtn.addEventListener("click", () => {
            vscode.postMessage({
              type: "viewCheckpointDiff",
              hash: body.checkpoint,
            });
          });
          diffRow.appendChild(viewBtn);
          const revertBtn = document.createElement("button");
          revertBtn.className = "btn-diff";
          revertBtn.textContent = "Revert";
          revertBtn.addEventListener("click", () => {
            vscode.postMessage({
              type: "revertCheckpoint",
              hash: body.checkpoint,
            });
          });
          diffRow.appendChild(revertBtn);
          el.appendChild(diffRow);
        }

        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "approval_request") {
        const el = document.createElement("div");
        el.className = "event-card approval";
        el.dataset.requestId = body.id || "";
        const summaryDiv = document.createElement("div");
        summaryDiv.className = "summary";
        summaryDiv.textContent =
          "Approval required: " + (body.summary || body.tool || "");
        el.appendChild(summaryDiv);
        const actions = document.createElement("div");
        actions.className = "actions";
        const approveBtn = document.createElement("button");
        approveBtn.className = "btn-approve";
        approveBtn.textContent = "Approve";
        approveBtn.addEventListener("click", () => {
          vscode.postMessage({
            type: "approve",
            requestId: body.id,
            approved: true,
          });
          approveBtn.disabled = true;
          denyBtn.disabled = true;
        });
        const denyBtn = document.createElement("button");
        denyBtn.className = "btn-deny";
        denyBtn.textContent = "Deny";
        denyBtn.addEventListener("click", () => {
          vscode.postMessage({
            type: "approve",
            requestId: body.id,
            approved: false,
          });
          approveBtn.disabled = true;
          denyBtn.disabled = true;
        });
        actions.appendChild(approveBtn);
        actions.appendChild(denyBtn);
        el.appendChild(actions);
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "approval_decision") {
        const requestId = body.id || "";
        const card = messagesEl.querySelector(
          '[data-request-id="' + requestId + '"]'
        );
        if (card) {
          const actions = card.querySelector(".actions");
          if (actions) actions.remove();
          const decision = document.createElement("div");
          decision.className =
            "decision " + (body.approved ? "approved" : "denied");
          decision.textContent = body.approved ? "✓ approved" : "✗ denied";
          card.appendChild(decision);
        } else {
          const el = document.createElement("div");
          el.className = "event-muted";
          el.textContent = body.approved ? "✓ approved" : "✗ denied";
          messagesEl.appendChild(el);
        }
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "usage") {
        const el = document.createElement("div");
        el.className = "event-muted";
        const pt = body.prompt_tokens ?? body.input_tokens ?? "?";
        const ct = body.completion_tokens ?? body.output_tokens ?? "?";
        el.textContent = "tokens: " + pt + " in / " + ct + " out";
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "dispatch") {
        const el = document.createElement("div");
        el.className = "event-card";
        const title = body.title || body.child || "?";
        const role = body.role || "";
        el.textContent = "⇄ dispatched " + title + " (" + role + ")";
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "steer") {
        const el = document.createElement("div");
        el.className = "event-card";
        el.textContent =
          "⤳ steer (" + (e.actor || "?") + "): " + (body.text || "");
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "context_inject") {
        const el = document.createElement("div");
        el.className = "event-card";
        el.textContent = "+ context injected";
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "pause" || kind === "resume" || kind === "interrupt") {
        const el = document.createElement("div");
        el.className = "event-muted";
        el.textContent = kind;
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "budget") {
        if (body.action === "raised") {
          const el = document.createElement("div");
          el.className = "event-muted";
          el.textContent =
            "budget raised to $" + (body.limit_usd ?? "?");
          messagesEl.appendChild(el);
          messagesEl.scrollTop = messagesEl.scrollHeight;
          return;
        }
        const el = document.createElement("div");
        el.className = "event-card budget";
        const spent = body.spent_usd ?? "?";
        const limit = body.limit_usd ?? "?";
        el.textContent =
          "budget exceeded: $" + spent + " ≥ $" + limit + " — paused";
        if (body.action === "paused") {
          const actions = document.createElement("div");
          actions.className = "actions";
          const raiseBtn = document.createElement("button");
          raiseBtn.type = "button";
          raiseBtn.textContent = "Raise budget…";
          raiseBtn.addEventListener("click", () => {
            const limitNum = typeof body.limit_usd === "number"
              ? body.limit_usd
              : parseFloat(String(body.limit_usd));
            if (Number.isFinite(limitNum)) {
              vscode.postMessage({ type: "raiseBudget", limitUsd: limitNum });
            }
          });
          actions.appendChild(raiseBtn);
          el.appendChild(actions);
        }
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "adopt") {
        const child = body.child || "?";
        const sha = body.merge_commit || "?";
        const shortSha = String(sha).slice(0, 7);
        const el = document.createElement("div");
        el.className = "event-card adopt";
        el.textContent = "✓ adopted " + child + " — merge " + shortSha;
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "verdict") {
        const decision = String(body.decision ?? "?");
        const oracleId = String(body.oracle_id ?? "?");
        const passed = decision === "pass";
        const el = document.createElement("div");
        el.className = "event-card verdict " + (passed ? "pass" : "fail");
        const label = document.createElement("div");
        label.textContent =
          "⚖ verdict: " + decision + " — " + oracleId;
        el.appendChild(label);
        if (body.journal_path) {
          const pathEl = document.createElement("div");
          pathEl.className = "journal-path";
          pathEl.textContent = String(body.journal_path);
          el.appendChild(pathEl);
        }
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }

      if (kind === "error") {
        if (body.escalated_to) {
          const el = document.createElement("div");
          el.className = "event-muted";
          el.textContent =
            "↷ escalated to " +
            body.escalated_to +
            ": " +
            (body.error || "");
          messagesEl.appendChild(el);
          messagesEl.scrollTop = messagesEl.scrollHeight;
        }
        return;
      }

      if (kind === "message" && e.actor && e.actor.startsWith("sub:")) {
        const el = document.createElement("div");
        el.className = "event-card report";
        const label = document.createElement("div");
        label.className = "report-label";
        label.textContent = "report from " + e.actor;
        el.appendChild(label);
        const bodyEl = document.createElement("div");
        bodyEl.innerHTML = linkify(body.text || "");
        bindFileLinks(bodyEl);
        el.appendChild(bodyEl);
        const childId = e.actor.slice(4);
        if (childId) {
          const adoptBtn = document.createElement("button");
          adoptBtn.className = "btn-adopt";
          adoptBtn.type = "button";
          adoptBtn.textContent = "Adopt work";
          adoptBtn.addEventListener("click", () => {
            vscode.postMessage({ type: "adoptWork", childId });
          });
          el.appendChild(adoptBtn);
        }
        messagesEl.appendChild(el);
        messagesEl.scrollTop = messagesEl.scrollHeight;
        return;
      }
    }

    function addMessage(role, text, pending, msgAttachments) {
      const el = document.createElement("div");
      el.className = "msg " + role + (pending ? " pending" : "");
      el.innerHTML = linkify(text);
      bindFileLinks(el);
      if (role === "user" && msgAttachments && msgAttachments.length > 0) {
        const chips = document.createElement("div");
        chips.className = "attach-chips";
        for (const att of msgAttachments) {
          const name = att.name || att.path || "?";
          const chip = document.createElement("span");
          chip.className = "attach-chip-inline";
          chip.textContent = "📎 " + name;
          chips.appendChild(chip);
        }
        el.appendChild(chips);
      }
      messagesEl.appendChild(el);
      messagesEl.scrollTop = messagesEl.scrollHeight;
      return el;
    }

    function clearPending() {
      if (pendingEl) {
        pendingEl.remove();
        pendingEl = null;
      }
    }

    function updatePending(text) {
      if (!pendingEl) {
        pendingEl = addMessage("assistant", text, true);
      } else {
        pendingEl.innerHTML = linkify(text);
        bindFileLinks(pendingEl);
      }
      messagesEl.scrollTop = messagesEl.scrollHeight;
    }

    window.addEventListener("message", (event) => {
      const msg = event.data;
      switch (msg.type) {
        case "reset":
          messagesEl.innerHTML = "";
          clearPending();
          for (const key of Object.keys(toolCallArgs)) {
            delete toolCallArgs[key];
          }
          break;
        case "init":
          messagesEl.innerHTML = "";
          updateSessionHeader(msg.session || "", msg.kind || "orchestrator");
          for (const e of msg.events || []) {
            renderEvent(e);
          }
          break;
        case "userMessage":
          clearPending();
          addMessage("user", msg.text, false, msg.attachments);
          break;
        case "filePicked":
          if (typeof msg.name === "string" && typeof msg.content === "string") {
            if (!attachments.some((a) => a.name === msg.name)) {
              attachments.push({ name: msg.name, content: msg.content });
              renderAttachments();
            }
          }
          break;
        case "event":
          renderEvent(msg.event);
          break;
        case "delta":
          updatePending(msg.text);
          break;
        case "assistantMessage":
          clearPending();
          addMessage("assistant", msg.text, false);
          break;
        case "error":
          clearPending();
          addMessage("assistant", "Error: " + msg.text, false);
          break;
      }
    });

    function doSend() {
      const text = inputEl.value.trim();
      if (!text) return;
      inputEl.value = "";
      const payload = { type: "send", text };
      if (attachments.length > 0) {
        payload.attachments = attachments.slice();
      }
      vscode.postMessage(payload);
      attachments = [];
      renderAttachments();
    }

    sendBtn.addEventListener("click", doSend);
    attachBtn.addEventListener("click", () => {
      vscode.postMessage({ type: "pickFile" });
    });
    document.getElementById("btn-pause").addEventListener("click", () => {
      vscode.postMessage({ type: "control", action: "pause" });
    });
    document.getElementById("btn-resume").addEventListener("click", () => {
      vscode.postMessage({ type: "control", action: "resume" });
    });
    document.getElementById("btn-steer").addEventListener("click", () => {
      vscode.postMessage({ type: "control", action: "steer" });
    });
    document.getElementById("btn-stop").addEventListener("click", () => {
      vscode.postMessage({ type: "control", action: "interrupt" });
    });
    inputEl.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        doSend();
      }
    });
  </script>
</body>
</html>`;
  }
}

function getNonce(): string {
  const chars =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  let nonce = "";
  for (let i = 0; i < 32; i++) {
    nonce += chars.charAt(Math.floor(Math.random() * chars.length));
  }
  return nonce;
}
