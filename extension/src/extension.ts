import * as vscode from "vscode";
import { ChatViewProvider } from "./chatView";

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

export function activate(context: vscode.ExtensionContext) {
  const shadowProvider = new ForgeShadowContentProvider();
  const provider = new ChatViewProvider(context, (fn) => {
    fileAtFn = fn;
  });

  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(
      "forge-shadow",
      shadowProvider
    ),
    vscode.window.registerWebviewViewProvider("forgeComposer.chat", provider),
    vscode.commands.registerCommand("forgeComposer.open", () => {
      void vscode.commands.executeCommand("forgeComposer.chat.focus");
    })
  );
}

export function deactivate() {
  fileAtFn = undefined;
}
