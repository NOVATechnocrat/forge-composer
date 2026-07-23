import * as vscode from "vscode";
import { DaemonClient, discover } from "./daemon";

interface ChatMessage {
  role: "user" | "assistant";
  text: string;
}

export class ChatViewProvider implements vscode.WebviewViewProvider {
  private view?: vscode.WebviewView;
  private client?: DaemonClient;
  private session?: string;
  private disposeStream?: () => void;
  private pendingDelta = "";

  constructor(private readonly context: vscode.ExtensionContext) {}

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

    webviewView.webview.onDidReceiveMessage((msg: { type?: string; text?: string }) => {
      if (msg.type === "send" && typeof msg.text === "string") {
        void this.handleSend(msg.text);
      }
    });

    webviewView.onDidDispose(() => {
      this.disposeStream?.();
      this.disposeStream = undefined;
      this.view = undefined;
    });

    void this.initSession(webviewView.webview);
  }

  private async initSession(webview: vscode.Webview): Promise<void> {
    if (!this.client) {
      return;
    }

    try {
      const sessions = await this.client.listSessions();
      this.session =
        sessions.length > 0 ? sessions[0]! : await this.client.createSession();

      const events = await this.client.events(this.session, 0);
      webview.postMessage({ type: "init", messages: eventsToMessages(events) });

      this.disposeStream?.();
      this.disposeStream = this.client.stream(
        this.session,
        (e) => this.onLedger(e),
        (t) => this.onDelta(t)
      );
    } catch (err) {
      webview.postMessage({
        type: "error",
        text: err instanceof Error ? err.message : "failed to connect",
      });
    }
  }

  private async handleSend(text: string): Promise<void> {
    if (!this.client || !this.session || !this.view) {
      return;
    }

    const trimmed = text.trim();
    if (!trimmed) {
      return;
    }

    this.pendingDelta = "";
    this.view.webview.postMessage({ type: "userMessage", text: trimmed });

    try {
      await this.client.sendMessage(this.session, trimmed);
    } catch (err) {
      this.view.webview.postMessage({
        type: "error",
        text: err instanceof Error ? err.message : "send failed",
      });
    }
  }

  private onLedger(e: {
    kind?: string;
    actor?: string;
    body?: { text?: string };
  }): void {
    if (!this.view || e.kind !== "message") {
      return;
    }

    const text = e.body?.text ?? "";
    if (e.actor === "orchestrator") {
      this.pendingDelta = "";
      this.view.webview.postMessage({ type: "assistantMessage", text });
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
  .msg.user {
    align-self: flex-end;
    background: var(--vscode-input-background);
  }
  .msg.assistant {
    align-self: flex-start;
    background: var(--vscode-editor-inactiveSelectionBackground);
  }
  .msg.pending { opacity: 0.75; }
  #input-area {
    display: flex;
    gap: 0.5rem;
    padding: 0.5rem;
    border-top: 1px solid var(--vscode-panel-border);
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
  <div id="messages"></div>
  <div id="input-area">
    <textarea id="input" rows="2" placeholder="Message Forge Composer…"></textarea>
    <button id="send">Send</button>
  </div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const messagesEl = document.getElementById("messages");
    const inputEl = document.getElementById("input");
    const sendBtn = document.getElementById("send");
    let pendingEl = null;

    function addMessage(role, text, pending) {
      const el = document.createElement("div");
      el.className = "msg " + role + (pending ? " pending" : "");
      el.textContent = text;
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
        pendingEl.textContent = text;
      }
      messagesEl.scrollTop = messagesEl.scrollHeight;
    }

    window.addEventListener("message", (event) => {
      const msg = event.data;
      switch (msg.type) {
        case "init":
          messagesEl.innerHTML = "";
          for (const m of msg.messages) {
            addMessage(m.role, m.text, false);
          }
          break;
        case "userMessage":
          clearPending();
          addMessage("user", msg.text, false);
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
      vscode.postMessage({ type: "send", text });
    }

    sendBtn.addEventListener("click", doSend);
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

function eventsToMessages(events: any[]): ChatMessage[] {
  const messages: ChatMessage[] = [];
  for (const e of events) {
    if (e.kind !== "message") {
      continue;
    }
    if (e.actor === "human") {
      messages.push({ role: "user", text: e.body?.text ?? "" });
    } else if (e.actor === "orchestrator") {
      messages.push({ role: "assistant", text: e.body?.text ?? "" });
    }
  }
  return messages;
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
