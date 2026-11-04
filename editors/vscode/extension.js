// The pico extension's code: starts the language server (`pico lsp`, PLAN P10.6b) for `.pi`
// files and restarts it on request or when its settings change.
"use strict";
const vscode = require("vscode");
const fs = require("fs");
const path = require("path");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

/** @type {LanguageClient | undefined} */
let client;

function serverOptions() {
  const conf = vscode.workspace.getConfiguration("pico");
  const command = conf.get("server.path") || "pico";
  const args = [...(conf.get("server.args") || []), "lsp"];
  return { command, args, transport: TransportKind.stdio };
}

async function start(context) {
  client = new LanguageClient("pico", "pico language server", serverOptions(), {
    documentSelector: [{ language: "pico" }],
    synchronize: {
      // (Other modules change on disk: the server re-checks open documents on save.)
      fileEvents: vscode.workspace.createFileSystemWatcher("**/{*.pi,project.json}"),
    },
  });
  try {
    await client.start();
  } catch (e) {
    const path = serverOptions().command;
    vscode.window.showErrorMessage(
      `pico: cannot start the language server (\`${path} lsp\`): ${e?.message ?? e}. ` +
        "Set `pico.server.path` to the pico binary.",
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

/**
 * The project around `dir`: the nearest directory with a project.json, and its `entry` (the
 * application's main file, spec §8.3) as an absolute path when it has one.
 * @param {string} dir
 * @returns {{ dir: string, entry?: string } | undefined}
 */
function projectOf(dir) {
  for (let d = dir; ; d = path.dirname(d)) {
    const manifest = path.join(d, "project.json");
    if (fs.existsSync(manifest)) {
      try {
        const json = JSON.parse(fs.readFileSync(manifest, "utf8"));
        return { dir: d, entry: typeof json.entry === "string" ? path.join(d, json.entry) : undefined };
      } catch {
        return { dir: d };
      }
    }
    if (path.dirname(d) === d) return undefined;
  }
}

/** The directory to start looking for the project in: the active file's, else the workspace's. */
function startDir() {
  const editor = vscode.window.activeTextEditor;
  if (editor && editor.document.uri.scheme === "file") return path.dirname(editor.document.uri.fsPath);
  const folder = vscode.workspace.workspaceFolders?.[0];
  return folder ? folder.uri.fsPath : undefined;
}

/** @param {vscode.ExtensionContext} context */
async function activate(context) {
  // The debugger (spec §14.5): `pico dap` builds the program and runs lldb-dap.
  context.subscriptions.push(
    vscode.debug.registerDebugAdapterDescriptorFactory("pico", {
      createDebugAdapterDescriptor() {
        const { command, args } = serverOptions();
        // (`serverOptions` ends with `lsp`: the same binary and arguments, then `dap`.)
        return new vscode.DebugAdapterExecutable(command, [...args.slice(0, -1), "dap"]);
      },
    }),
    vscode.debug.registerDebugConfigurationProvider("pico", {
      // F5 without a launch.json: the project's `entry` (its main file), else the current
      // file.
      resolveDebugConfiguration(_folder, config) {
        if (!config.type && !config.request && !config.name) {
          const dir = startDir();
          const project = dir ? projectOf(dir) : undefined;
          if (project?.entry) {
            return { type: "pico", request: "launch", name: "Debug the project", program: project.entry, cwd: project.dir };
          }
          const editor = vscode.window.activeTextEditor;
          if (editor && editor.document.languageId === "pico") {
            return { type: "pico", request: "launch", name: "Debug the current pico file", program: "${file}" };
          }
        }
        return config;
      },
    }),
  );
  // Run Build Task: `pico build` in the project's directory (it builds the `entry`).
  context.subscriptions.push(
    vscode.tasks.registerTaskProvider("pico", {
      provideTasks() {
        const dir = startDir();
        const project = dir ? projectOf(dir) : undefined;
        if (!project?.entry) return [];
        const { command, args } = serverOptions();
        const definition = { type: "pico", command: "build" };
        const task = new vscode.Task(
          definition,
          vscode.TaskScope.Workspace,
          "build",
          "pico",
          new vscode.ProcessExecution(command, [...args.slice(0, -1), "build"], { cwd: project.dir }),
          [],
        );
        task.group = vscode.TaskGroup.Build;
        return [task];
      },
      resolveTask(task) {
        return task;
      },
    }),
  );
  context.subscriptions.push(
    vscode.commands.registerCommand("pico.restartServer", async () => {
      await stop();
      await start(context);
    }),
    vscode.workspace.onDidChangeConfiguration(async (e) => {
      if (e.affectsConfiguration("pico.server")) {
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
