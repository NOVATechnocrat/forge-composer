import * as vscode from "vscode";
import { ChatViewProvider } from "./chatView";

export function activate(context: vscode.ExtensionContext) {
  const provider = new ChatViewProvider(context);

  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider("forgeComposer.chat", provider),
    vscode.commands.registerCommand("forgeComposer.open", () => {
      void vscode.commands.executeCommand("forgeComposer.chat.focus");
    })
  );
}

export function deactivate() {}
