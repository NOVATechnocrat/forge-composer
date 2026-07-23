import * as vscode from "vscode";

export function activate(context: vscode.ExtensionContext) {
  context.subscriptions.push(
    vscode.commands.registerCommand("forgeComposer.open", () => {
      vscode.window.showInformationMessage(
        "Forge Composer M0 scaffold — daemon connection lands in M0 build."
      );
    })
  );
}

export function deactivate() {}
