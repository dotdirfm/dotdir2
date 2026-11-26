import { MessageChannel, Worker } from 'node:worker_threads';

function send(message) {
  const payload = JSON.stringify({ jsonrpc: '2.0', ...message });
  process.stdout.write('Content-Length: ' + Buffer.byteLength(payload) + '\r\n\r\n' + payload);
}

let worker;
let stopped = false;
let nextRequest = 0;
let refresh;
const fileRequests = new Map();

function initialize(params) {
  const { port1, port2 } = new MessageChannel();
  worker = new Worker(new URL('data:text/javascript,' + encodeURIComponent(
    '(' + analyze.toString() + ')().catch(error => { console.error(error); process.exit(1); })'
  )), {
    workerData: { params, port: port2 },
    transferList: [port2],
    execArgv: [],
  });
  port1.on('message', ({ operation, path, signal }) => {
    const id = 'archive:' + ++nextRequest;
    fileRequests.set(id, { port: port1, signal });
    send({ id, method: 'dotdir/archiveFile', params: { operation, path } });
  });
  worker.on('message', message => {
    if (message.method === 'workspace/diagnostic/refresh') {
      clearTimeout(refresh);
      refresh = setTimeout(() => send(message), 120);
    } else {
      send(message);
    }
  });
  worker.on('error', error => send({ method: 'dotdir/activationFailed', params: { message: error.message } }));
  worker.on('exit', code => {
    if (!stopped) {
      send({ method: 'dotdir/activationFailed', params: { message: 'TypeScript archive worker exited (' + code + ')' } });
    }
    port1.close();
  });
}

function dispatch(message) {
  if (fileRequests.has(message.id)) {
    const { port, signal } = fileRequests.get(message.id);
    fileRequests.delete(message.id);
    port.postMessage({ result: message.result, error: message.error });
    Atomics.store(signal, 0, 1);
    Atomics.notify(signal, 0);
  } else if (message.method === 'initialize') {
    initialize(message.params);
    worker.postMessage(message);
  } else if (message.method === 'exit') {
    stopped = true;
    process.exit(0);
  } else if (message.method) {
    worker?.postMessage(message);
  }
}

let input = Buffer.alloc(0);
let length;
process.stdin.on('data', chunk => {
  input = Buffer.concat([input, chunk]);
  for (;;) {
    if (length === undefined) {
      const end = input.indexOf('\r\n\r\n');
      if (end < 0 && input.length < 1024) return;
      const match = /^Content-Length:\s*(\d+)\s*$/im.exec(input.subarray(0, end).toString());
      length = Number(match?.[1]);
      if (end < 0 || !Number.isSafeInteger(length) || length > 128 * 1024 * 1024) {
        throw new Error('Invalid TypeScript archive message length');
      }
      input = input.subarray(end + 4);
    }
    if (input.length < length) return;
    const message = JSON.parse(input.subarray(0, length));
    input = input.subarray(length);
    length = undefined;
    dispatch(message);
  }
});
process.stdin.on('end', () => { stopped = true; process.exit(0); });

async function analyze() {
  const { parentPort, workerData, receiveMessageOnPort } = await import('node:worker_threads');
  const { createRequire } = await import('node:module');
  const { dirname, join } = await import('node:path');
  const { readdirSync, existsSync } = await import('node:fs');
  const { fileURLToPath, pathToFileURL } = await import('node:url');
  const { params, port } = workerData;
  const extension = params.initializationOptions.extensionRoot;
  const require = createRequire(join(extension, 'package.json'));
  const sdk = dirname(require.resolve('typescript/package.json'));
  const { API, DiagnosticCategory, SymbolFlags } = await import(pathToFileURL(require.resolve('typescript/unstable/async')));
  const { fsCallbackNames } = await import(pathToFileURL(require.resolve('typescript/unstable/fs')));
  const root = fileURLToPath(params.rootUri);
  const slash = path => process.platform === 'win32' ? path.replaceAll('\\', '/') : path;
  const workspaceRoot = slash(root);
  let inferredPath;
  let inferredConfig;
  function relativePath(path) {
    const normalized = slash(path);
    if (normalized === workspaceRoot) return '';
    if (normalized.startsWith(workspaceRoot + '/')) return normalized.slice(workspaceRoot.length + 1);
  }

  function readArchive(operation, path) {
    const relative = relativePath(path);
    if (relative === undefined) return undefined;
    if (slash(path) === inferredPath) {
      if (operation === 'readFile') return inferredConfig ?? null;
      if (operation === 'fileExists') return inferredConfig !== undefined;
      if (operation === 'directoryExists') return false;
      if (operation === 'realpath') return path;
      return { files: [], directories: [] };
    }
    // The SDK's callbacks return synchronously. Only this worker waits; the
    // parent keeps reading replies and sends Rust's VFS result over the port.
    const signal = new Int32Array(new SharedArrayBuffer(4));
    port.postMessage({ operation, path: relative, signal });
    if (Atomics.wait(signal, 0, 0, 12_000) === 'timed-out') {
      throw new Error('Archive filesystem request timed out');
    }
    const response = receiveMessageOnPort(port)?.message;
    if (!response) throw new Error('Archive filesystem response is missing');
    if (response.error) throw new Error(response.error.message);
    return response.result;
  }

  const packages = join(sdk, '..', '@typescript');
  const prefix = 'typescript-' + process.platform + '-';
  const installed = existsSync(packages) ? readdirSync(packages) : [];
  const executable = process.platform === 'win32' ? 'tsc.exe' : 'tsc';
  const server = [prefix + process.arch, ...installed.filter(name => name.startsWith(prefix))]
    .map(name => join(packages, name, 'lib', executable))
    .find(path => existsSync(path));
  const fs = Object.fromEntries(fsCallbackNames.map(name => [name, path => readArchive(name, path)]));
  const api = new API({ cwd: root, fs, tsserverPath: server });

  const documents = new Map();
  const openFiles = new Set();
  const closeFiles = new Set();
  const changed = new Set();
  let projects = new Set();
  let snapshot;
  let tail = Promise.resolve();

  const notify = (method, params) => parentPort.postMessage({ method, params });
  const reply = (id, result) => parentPort.postMessage({ id, result: result ?? null });

  async function updateSnapshot() {
    if (snapshot && !changed.size && !openFiles.size && !closeFiles.size) return;
    const current = new Set([...documents.values()].map(document => document.config));
    const inferredFiles = [...documents.entries()]
      .filter(([, document]) => document.config === inferredPath)
      .map(([file]) => file);
    const inferred = inferredFiles.length ? JSON.stringify({
      compilerOptions: { allowJs: true, noEmit: true },
      files: inferredFiles,
    }) : undefined;
    if (inferred !== inferredConfig) {
      inferredConfig = inferred;
      if (projects.has(inferredPath) && current.has(inferredPath)) changed.add(inferredPath);
    }
    const previous = snapshot;
    // Native Preview 7.0 only applies reported VFS file changes to projects
    // opened explicitly through the API, including our inferred configuration.
    snapshot = await api.updateSnapshot({
      openFiles: [...openFiles], closeFiles: [...closeFiles], fileChanges: { changed: [...changed] },
      openProjects: [...current].filter(project => !projects.has(project)),
      closeProjects: [...projects].filter(project => !current.has(project)),
    });
    projects = current;
    changed.clear();
    openFiles.clear();
    closeFiles.clear();
    await previous?.dispose();
  }

  function findConfig(file) {
    for (let directory = dirname(file); relativePath(directory) !== undefined; directory = dirname(directory)) {
      for (const name of ['tsconfig.json', 'jsconfig.json']) {
        const config = slash(join(directory, name));
        if (readArchive('fileExists', config)) return config;
      }
    }
    if (!inferredPath) {
      let name = '.dotdir-inferred';
      while (readArchive('fileExists', slash(join(root, name + '.json')))) name += '-';
      inferredPath = slash(join(root, name + '.json'));
    }
    return inferredPath;
  }

  function positionOffset(text, position) {
    let offset = 0;
    for (let line = 0; line < position.line; line++) {
      const next = text.indexOf('\n', offset);
      if (next < 0) return text.length;
      offset = next + 1;
    }
    return Math.min(text.length, offset + position.character);
  }

  const severity = new Map([
    [DiagnosticCategory.Error, 1], [DiagnosticCategory.Warning, 2],
    [DiagnosticCategory.Message, 3], [DiagnosticCategory.Suggestion, 4],
  ]);
  const range = (source, start, end) => ({
    start: source.getLineAndCharacterOfPosition(start),
    end: source.getLineAndCharacterOfPosition(end),
  });

  async function query(method, params) {
    const file = fileURLToPath(params.textDocument.uri);
    const document = documents.get(file);
    if (!document) return null;
    await updateSnapshot();
    const configured = snapshot.getProject(document.config);
    const project = await configured?.program.getSourceFileMetadata(file)
      ? configured : await snapshot.getDefaultProjectForFile(file);
    if (!project) return method === 'textDocument/diagnostic' ? { kind: 'full', items: [] } : null;
    const { program, checker } = project;
    if (method === 'textDocument/diagnostic') {
      const source = await program.getSourceFile(file);
      if (!source) return { kind: 'full', items: [] };
      const items = [...await program.getSyntacticDiagnostics(file), ...await program.getSemanticDiagnostics(file)]
        .map(item => ({
          range: range(source, item.pos, item.end), severity: severity.get(item.category),
          code: item.code, source: 'TypeScript', message: item.text,
        }));
      return { kind: 'full', items };
    }
    if (!params.position) throw new Error('Unsupported archive language request ' + method);
    const offset = positionOffset(document.text, params.position);
    if (method === 'textDocument/completion') {
      const completion = await checker.getCompletionsAtPosition(file, offset, {
        triggerCharacter: params.context?.triggerCharacter,
      });
      if (!completion) return [];
      return {
        isIncomplete: completion.isIncomplete,
        items: completion.entries.map(entry => ({
          label: entry.name, detail: entry.detail, sortText: entry.sortText,
          insertText: entry.insertText, filterText: entry.filterText,
        })),
      };
    }
    let symbol = await checker.getSymbolAtPosition(file, offset);
    if (!symbol) return null;
    if (symbol.flags & SymbolFlags.Alias) symbol = await checker.getAliasedSymbol(symbol);
    if (method === 'textDocument/definition') {
      const targets = [];
      for (const declaration of symbol.declarations.slice(0, 25)) {
        const node = await declaration.resolve(project);
        if (node) {
          const source = node.getSourceFile();
          const target = node.name ?? node;
          targets.push({ uri: pathToFileURL(source.fileName).href, range: range(source, target.getStart(source), target.getEnd()) });
        }
      }
      return targets;
    }
    if (method === 'textDocument/hover') {
      const type = await checker.getTypeAtPosition(file, offset);
      const description = type ? symbol.name + ': ' + await checker.typeToString(type) : symbol.name;
      const documentation = await checker.getDocumentationCommentOfSymbol(symbol);
      return { contents: { kind: 'plaintext', value: [description, documentation].filter(Boolean).join('\n\n') } };
    }
    throw new Error('Unsupported archive language request ' + method);
  }

  async function handle({ id, method, params }) {
    switch (method) {
      case 'initialize':
        return reply(id, { capabilities: {
          textDocumentSync: 2, completionProvider: { triggerCharacters: ['.'] },
          hoverProvider: true, definitionProvider: true,
          diagnosticProvider: { interFileDependencies: true, workspaceDiagnostics: false },
        } });
      case 'shutdown':
        await api.close();
        return reply(id, null);
      case 'textDocument/didOpen': {
        const file = fileURLToPath(params.textDocument.uri);
        documents.set(file, { text: params.textDocument.text, config: findConfig(file) });
        closeFiles.delete(file);
        openFiles.add(file);
        await updateSnapshot();
        notify('dotdir/documentReady', { uri: params.textDocument.uri });
        return;
      }
      case 'textDocument/didClose': {
        const file = fileURLToPath(params.textDocument.uri);
        documents.delete(file);
        openFiles.delete(file);
        closeFiles.add(file);
        return;
      }
      case 'textDocument/didChange': {
        const file = fileURLToPath(params.textDocument.uri);
        const document = documents.get(file);
        if (!document) return;
        let text = document.text;
        for (const change of params.contentChanges) {
          text = change.range
            ? text.slice(0, positionOffset(text, change.range.start)) + change.text + text.slice(positionOffset(text, change.range.end))
            : change.text;
        }
        document.text = text;
        changed.add(file);
        return;
      }
      case 'dotdir/archiveChanged':
        changed.add(fileURLToPath(params.uri));
        notify('workspace/diagnostic/refresh');
        return;
      default:
        if (id !== undefined) reply(id, await query(method, params));
    }
  }

  parentPort.on('message', message => {
    tail = tail.then(async () => {
      try {
        await handle(message);
      } catch (error) {
        if (message.id !== undefined) {
          parentPort.postMessage({ id: message.id, error: { code: -32000, message: error.message } });
        } else {
          notify('dotdir/activationFailed', { message: error.message, stack: error.stack });
        }
      }
    });
  });
}
