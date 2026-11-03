// The ppl extension's code: starts the language server (`ppl lsp`, PLAN P10.6b) for `.ppl`
// files and restarts it on request or when its settings change.
"use strict";
const vscode = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

/** @type {LanguageClient | undefined} */
let client;

function serverOptions() {
  const conf = vscode.workspace.getConfiguration("ppl");
  const command = conf.get("server.path") || "ppl";
  const args = [...(conf.get("server.args") || []), "lsp"];
  return { command, args, transport: TransportKind.stdio };
}

async function start(context) {
  client = new LanguageClient("ppl", "ppl language server", serverOptions(), {
    documentSelector: [{ language: "ppl" }],
    synchronize: {
      // (Other modules change on disk: the server re-checks open documents on save.)
      fileEvents: vscode.workspace.createFileSystemWatcher("**/{*.ppl,project.json}"),
    },
  });
  try {
    await client.start();
  } catch (e) {
    const path = serverOptions().command;
    vscode.window.showErrorMessage(
      `ppl: cannot start the language server (\`${path} lsp\`): ${e?.message ?? e}. ` +
        "Set `ppl.server.path` to the ppl binary.",
    );
  }
}

async function stop() {
  if (client) {
    const c = client;
    client = undefined;
    try {
      await c.stop();
    } catch {
      // (It may not have started.)
    }
  }
}

/** @param {vscode.ExtensionContext} context */
async function activate(context) {
  // The debugger (spec §14.5): `ppl dap` builds the program and runs lldb-dap.
  context.subscriptions.push(
    vscode.debug.registerDebugAdapterDescriptorFactory("ppl", {
      createDebugAdapterDescriptor() {
        const { command, args } = serverOptions();
        // (`serverOptions` ends with `lsp`: the same binary and arguments, then `dap`.)
        return new vscode.DebugAdapterExecutable(command, [...args.slice(0, -1), "dap"]);
      },
    }),
    vscode.debug.registerDebugConfigurationProvider("ppl", {
      // F5 without a launch.json: debug the current file.
      resolveDebugConfiguration(_folder, config) {
        if (!config.type && !config.request && !config.name) {
          const editor = vscode.window.activeTextEditor;
          if (editor && editor.document.languageId === "ppl") {
            return { type: "ppl", request: "launch", name: "Debug the current ppl file", program: "${file}" };
          }
        }
        return config;
      },
    }),
  );
  context.subscriptions.push(
    vscode.commands.registerCommand("ppl.restartServer", async () => {
      await stop();
      await start(context);
    }),
    vscode.workspace.onDidChangeConfiguration(async (e) => {
      if (e.affectsConfiguration("ppl.server")) {
        await stop();
        await start(context);
      }
    }),
  );
  await start(context);
}

function deactivate() {
  return stop();
}

module.exports = { activate, deactivate };
