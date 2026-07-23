import * as vscode from "vscode";
import { ChatViewProvider } from "./chatView";
import { DaemonClient } from "./daemon";
import { SessionsBoardProvider } from "./sessionsBoard";

type FileAtFn = (
  session: string,
  hash: string,
  path: string
) => Promise<string>;

let fileAtFn: FileAtFn | undefined;

class ForgeShadowContentProvider implements vscode.TextDocumentContentProvider {
  provideTextDocumentContent(uri: vscode.Uri): Thenable<string> {
    if (!fileAtFn) {
      return Promise.resolve("");
    }
    const trimmed = uri.path.replace(/^\//, "");
    const slash1 = trimmed.indexOf("/");
    const slash2 = trimmed.indexOf("/", slash1 + 1);
    if (slash1 === -1 || slash2 === -1) {
      return Promise.resolve("");
    }
    const session = trimmed.slice(0, slash1);
    const hash = trimmed.slice(slash1 + 1, slash2);
    const relPath = decodeURIComponent(trimmed.slice(slash2 + 1));
    return fileAtFn(session, hash, relPath);
  }
}

async function resolveSessionId(
  client: DaemonClient,
  id?: string
): Promise<string | undefined> {
  if (id) {
    return id;
  }
  const sessions = await client.sessionsDetail();
  if (sessions.length === 0) {
    void vscode.window.showWarningMessage("No sessions available.");
    return undefined;
  }
  const picked = await vscode.window.showQuickPick(
    sessions.map((s) => ({
      label: s.title ?? s.id.slice(-8),
      description: `${s.kind} · ${s.status}`,
      id: s.id,
    })),
    { placeHolder: "Select session" }
  );
  return picked?.id;
}

export function activate(context: vscode.ExtensionContext) {
  const shadowProvider = new ForgeShadowContentProvider();
  const chatProvider = new ChatViewProvider(context, (fn) => {
    fileAtFn = fn;
  });
  const board = new SessionsBoardProvider();

  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(
      "forge-shadow",
      shadowProvider
    ),
    vscode.window.registerWebviewViewProvider(
      "forgeComposer.chat",
      chatProvider
    ),
    vscode.window.createTreeView("forgeComposer.sessions", {
      treeDataProvider: board,
    }),
    board
  );

  board.startPolling();

  const clientOrWarn = (): DaemonClient | undefined => {
    const client =
      chatProvider.getClient() ?? board.getClient();
    if (!client) {
      void vscode.window.showErrorMessage(
        "composerd is not running — start it with composerd serve"
      );
    }
    return client;
  };

  const refreshBoard = () => void board.refresh();

  context.subscriptions.push(
    vscode.commands.registerCommand("forgeComposer.open", () => {
      void vscode.commands.executeCommand("forgeComposer.chat.focus");
    }),
    vscode.commands.registerCommand(
      "forgeComposer.openSession",
      (id: string) => {
        void chatProvider.openSession(id);
        void vscode.commands.executeCommand("forgeComposer.chat.focus");
      }
    ),
    vscode.commands.registerCommand(
      "forgeComposer.pauseSession",
      async (id?: string) => {
        const client = clientOrWarn();
        if (!client) {
          return;
        }
        const sessionId = await resolveSessionId(client, id);
        if (!sessionId) {
          return;
        }
        try {
          await client.pause(sessionId);
          refreshBoard();
        } catch (err) {
          void vscode.window.showErrorMessage(
            err instanceof Error ? err.message : "pause failed"
          );
        }
      }
    ),
    vscode.commands.registerCommand(
      "forgeComposer.resumeSession",
      async (id?: string) => {
        const client = clientOrWarn();
        if (!client) {
          return;
        }
        const sessionId = await resolveSessionId(client, id);
        if (!sessionId) {
          return;
        }
        try {
          await client.resume(sessionId);
          refreshBoard();
        } catch (err) {
          void vscode.window.showErrorMessage(
            err instanceof Error ? err.message : "resume failed"
          );
        }
      }
    ),
    vscode.commands.registerCommand(
      "forgeComposer.steerSession",
      async (id?: string) => {
        const client = clientOrWarn();
        if (!client) {
          return;
        }
        const sessionId = await resolveSessionId(client, id);
        if (!sessionId) {
          return;
        }
        const text = await vscode.window.showInputBox({
          prompt: "Steer text",
        });
        if (!text) {
          return;
        }
        try {
          await client.steer(sessionId, text);
          refreshBoard();
        } catch (err) {
          void vscode.window.showErrorMessage(
            err instanceof Error ? err.message : "steer failed"
          );
        }
      }
    ),
    vscode.commands.registerCommand(
      "forgeComposer.injectContext",
      async (id?: string) => {
        const client = clientOrWarn();
        if (!client) {
          return;
        }
        const sessionId = await resolveSessionId(client, id);
        if (!sessionId) {
          return;
        }
        const text = await vscode.window.showInputBox({
          prompt: "Context to inject",
        });
        if (!text) {
          return;
        }
        try {
          await client.inject(sessionId, text);
          refreshBoard();
        } catch (err) {
          void vscode.window.showErrorMessage(
            err instanceof Error ? err.message : "inject failed"
          );
        }
      }
    ),
    vscode.commands.registerCommand(
      "forgeComposer.interruptSession",
      async (id?: string) => {
        const client = clientOrWarn();
        if (!client) {
          return;
        }
        const sessionId = await resolveSessionId(client, id);
        if (!sessionId) {
          return;
        }
        try {
          await client.interrupt(sessionId);
          refreshBoard();
        } catch (err) {
          void vscode.window.showErrorMessage(
            err instanceof Error ? err.message : "interrupt failed"
          );
        }
      }
    )
  );
}

export function deactivate() {
  fileAtFn = undefined;
}
