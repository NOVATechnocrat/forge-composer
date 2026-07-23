import * as vscode from "vscode";
import { DaemonClient, discover, SessionDetail } from "./daemon";

export class SessionNode {
  constructor(
    public readonly detail: SessionDetail,
    public readonly children: SessionNode[] = []
  ) {}
}

export class SessionsBoardProvider implements vscode.TreeDataProvider<SessionNode>, vscode.Disposable {
  private readonly _onDidChangeTreeData =
    new vscode.EventEmitter<SessionNode | undefined>();
  readonly onDidChangeTreeData = this._onDidChangeTreeData.event;

  private client?: DaemonClient;
  private sessions: SessionDetail[] = [];
  private pollTimer?: ReturnType<typeof setInterval>;
  private readonly disposables: vscode.Disposable[] = [];

  constructor() {
    const info = discover();
    if (info) {
      this.client = new DaemonClient(info);
    }

    this.disposables.push(
      vscode.commands.registerCommand(
        "forgeComposer.adoptWork",
        (node: SessionNode) => {
          void this.handleAdopt(node);
        }
      )
    );
  }

  getClient(): DaemonClient | undefined {
    return this.client;
  }

  startPolling(): void {
    void this.refresh();
    this.pollTimer = setInterval(() => void this.refresh(), 3000);
  }

  dispose(): void {
    if (this.pollTimer !== undefined) {
      clearInterval(this.pollTimer);
      this.pollTimer = undefined;
    }
    for (const d of this.disposables) {
      d.dispose();
    }
  }

  private async handleAdopt(node: SessionNode): Promise<void> {
    if (!this.client) {
      return;
    }

    const child = node.detail;
    if (child.kind !== "subagent" || !child.parent) {
      return;
    }

    const branch = `fc/${child.id.toLowerCase()}`;
    const confirmed = await vscode.window.showWarningMessage(
      `Merge ${branch} into your branch and remove the worktree?`,
      { modal: true },
      "Adopt work"
    );
    if (confirmed !== "Adopt work") {
      return;
    }

    try {
      await this.client.adopt(child.parent, child.id);
      await this.refresh();
      void vscode.window.showInformationMessage("Work adopted.");
    } catch (err) {
      void vscode.window.showErrorMessage(
        err instanceof Error ? err.message : "adopt failed"
      );
    }
  }

  async refresh(): Promise<void> {
    if (!this.client) {
      return;
    }
    try {
      this.sessions = await this.client.sessionsDetail();
      this._onDidChangeTreeData.fire(undefined);
    } catch {
      // ignore transient poll failures
    }
  }

  getTreeItem(element: SessionNode): vscode.TreeItem {
    const d = element.detail;
    const label = d.title ?? d.id.slice(-8);
    const rolePrefix = d.kind === "subagent" ? `${d.role} · ` : "";
    const totalTok = d.prompt_tokens + d.completion_tokens;
    const description = `${rolePrefix}${d.status} · ${totalTok} tok · $${d.cost_usd.toFixed(4)}`;

    const icon =
      d.status === "running"
        ? "debug-start"
        : d.status === "paused"
          ? "debug-pause"
          : "circle-outline";

    const collapsible =
      element.children.length > 0
        ? vscode.TreeItemCollapsibleState.Collapsed
        : vscode.TreeItemCollapsibleState.None;

    const item = new vscode.TreeItem(label, collapsible);
    item.description = description;
    item.iconPath = new vscode.ThemeIcon(icon);
    item.command = {
      command: "forgeComposer.openSession",
      title: "Open Session",
      arguments: [d.id],
    };
    if (d.kind === "subagent" && d.status === "idle") {
      item.contextValue = "subagentIdle";
    }
    return item;
  }

  getChildren(element?: SessionNode): SessionNode[] {
    if (element) {
      return element.children;
    }

    const orchestrators = this.sessions.filter((s) => s.parent === null);
    return orchestrators.map((o) => {
      const children = this.sessions
        .filter((s) => s.parent === o.id)
        .map((s) => new SessionNode(s));
      return new SessionNode(o, children);
    });
  }
}
