import { existsSync, readFileSync, mkdirSync, watch, writeFileSync } from 'node:fs';
import * as fs from 'node:fs/promises';
import { matchesGlob, posix } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';

class Disposable {
  constructor(dispose) { this.dispose = dispose; }
  static from(...items) { return new Disposable(() => items.forEach(item => item.dispose())); }
}

class EventEmitter {
  listeners = new Set();
  event = (listener, receiver, subscriptions) => {
    const callback = event => listener.call(receiver, event);
    this.listeners.add(callback);
    const disposable = new Disposable(() => this.listeners.delete(callback));
    subscriptions?.push(disposable);
    return disposable;
  };
  fire(event) {
    for (const listener of this.listeners) {
      try { listener(event); } catch (error) { console.error(error); }
    }
  }
  dispose() { this.listeners.clear(); }
}

class Uri {
  constructor(url) { this.url = url; }
  static parse(value) { return new Uri(new URL(value)); }
  static file(path) { return new Uri(pathToFileURL(path)); }
  static from({ scheme, authority = '', path = '', query = '', fragment = '' }) {
    const host = authority || scheme === 'file' ? '//' + authority : '';
    const encodedPath = path.split('/').map(encodeURIComponent).join('/');
    const search = query ? '?' + encodeURIComponent(query) : '';
    const hash = fragment ? '#' + encodeURIComponent(fragment) : '';
    return Uri.parse(scheme + ':' + host + encodedPath + search + hash);
  }
  static joinPath(uri, ...parts) { return uri.with({ path: posix.join(uri.path, ...parts) }); }
  get fsPath() { return this.scheme === 'file' ? fileURLToPath(this.url) : this.path; }
  get scheme() { return this.url.protocol.slice(0, -1); }
  get authority() { return this.url.host; }
  get path() { return decodeURIComponent(this.url.pathname); }
  get query() { return decodeURIComponent(this.url.search.slice(1)); }
  get fragment() { return decodeURIComponent(this.url.hash.slice(1)); }
  with(change) {
    const url = new URL(this.url);
    if (change.scheme !== undefined) url.protocol = change.scheme + ':';
    if (change.authority !== undefined) url.host = change.authority;
    if (change.path !== undefined) url.pathname = change.path.split('/').map(encodeURIComponent).join('/');
    if (change.query !== undefined) url.search = encodeURIComponent(change.query);
    if (change.fragment !== undefined) url.hash = encodeURIComponent(change.fragment);
    return new Uri(url);
  }
  toString() { return this.url.href; }
  toJSON() { return { scheme: this.scheme, authority: this.authority, path: this.path, query: this.query, fragment: this.fragment }; }
}

class MarkdownString {
  constructor(value = '') { this.value = value; }
  appendMarkdown(value) { this.value += value; return this; }
  appendText(value) { return this.appendMarkdown(value.replace(/[\\`*_{}\[\]()#+.!<>~-]/g, '\\$&')); }
  appendCodeblock(value, language = '') { return this.appendMarkdown('\n```' + language + '\n' + value + '\n```\n'); }
}

class CancellationTokenSource {
  constructor() {
    this.cancelled = new EventEmitter();
    this.token = { isCancellationRequested: false, onCancellationRequested: this.cancelled.event };
  }
  cancel() {
    if (!this.token.isCancellationRequested) {
      this.token.isCancellationRequested = true;
      this.cancelled.fire();
    }
  }
  dispose() { this.cancelled.dispose(); }
}

class TabInputText {
  constructor(uri) { this.uri = uri; }
}

function enumValues(names, first = 0) {
  return Object.fromEntries(names.flatMap((name, i) => [[name, first + i], [first + i, name]]));
}

const emptyEvent = () => new Disposable(() => {});
const providerKinds = new Map([
  ['textDocument/hover', ['Hover', 'provideHover']],
  ['textDocument/completion', ['CompletionItem', 'provideCompletionItems']],
  ['textDocument/definition', ['Definition', 'provideDefinition']]
]);

function documentMatches(selector, document) {
  return (Array.isArray(selector) ? selector : [selector]).some(item => {
    if (typeof item === 'string') return item === '*' || item === document.languageId;
    if (!item) return false;
    return (!item.language || item.language === '*' || item.language === document.languageId)
      && (!item.scheme || item.scheme === '*' || item.scheme === document.uri.scheme)
      && (!item.pattern || pathMatches(item.pattern, document.uri.fsPath));
  });
}

function pathMatches(glob, path) {
  if (typeof glob !== 'string') {
    const base = glob.baseUri?.fsPath ?? glob.base;
    if (!base) return false;
    path = relative(base, path);
    if (path === '..' || path.startsWith('..' + sep) || isAbsolute(path)) return false;
    glob = glob.pattern;
  }
  return matchesGlob(path.split(sep).join('/'), glob) || (isAbsolute(path) && matchesGlob(path.slice(1).split(sep).join('/'), glob));
}

function getConfiguration(values, section, overrides = {}) {
  function valueFor(key) {
    if (Object.hasOwn(values, key)) return values[key];
    const prefix = key ? key + '.' : '';
    const result = {};
    for (const [name, value] of Object.entries(values)) {
      if (!name.startsWith(prefix)) continue;
      const parts = name.slice(prefix.length).split('.');
      const last = parts.pop();
      let object = result;
      for (const part of parts) {
        if (['__proto__', 'constructor', 'prototype'].includes(part)) {
          object = null;
          break;
        }
        if (!Object.hasOwn(object, part) || typeof object[part] !== 'object' || object[part] === null) {
          object[part] = {};
        }
        object = object[part];
      }
      if (object && !['__proto__', 'constructor', 'prototype'].includes(last)) object[last] = structuredClone(value);
    }
    return Object.keys(result).length ? result : undefined;
  }
  const keyFor = key => [section, key].filter(Boolean).join('.');
  const result = Object.assign({}, valueFor(section ?? ''));
  Object.defineProperties(result, {
    get: { value: (key, fallback) => valueFor(keyFor(key)) ?? fallback },
    has: { value: key => valueFor(keyFor(key)) !== undefined },
    inspect: { value: key => ({
      key: keyFor(key), defaultValue: valueFor(keyFor(key)), globalValue: overrides[keyFor(key)]
    }) }
  });
  return result;
}

function memento(directory, filename) {
  const path = resolve(directory, filename);
  let data = {};
  try { data = JSON.parse(readFileSync(path, 'utf8')); } catch {}
  return {
    get: (key, fallback) => Object.hasOwn(data, key) ? data[key] : fallback,
    keys: () => Object.keys(data),
    async update(key, value) {
      if (value === undefined) delete data[key];
      else Object.defineProperty(data, key, { value, enumerable: true, configurable: true, writable: true });
      mkdirSync(directory, { recursive: true });
      writeFileSync(path, JSON.stringify(data), { mode: 0o600 });
    },
    setKeysForSync() {}
  };
}

class LanguageSession {
  documents = new Map();
  providers = new Map();
  open = new EventEmitter();
  change = new EventEmitter();
  close = new EventEmitter();
  save = new EventEmitter();
  active = new EventEmitter();
  contentProviders = new Map();
  readers = new Set();
  diagnosticCollections = new Set();
  folders = [];
  requests = new Map();

  constructor(api) {
    const session = this;
    const group = {
      get activeTab() {
        return session.editor && { input: new TabInputText(session.editor.document.uri), group: this, isActive: true };
      },
      get tabs() { return [...session.documents.values()].map(document => ({ input: new TabInputText(document.uri), group: this })); },
      isActive: true,
      viewColumn: 1
    };
    const alerts = message => { console.error(message); return Promise.resolve(undefined); };
    Object.assign(api.window, {
      onDidChangeActiveTextEditor: this.active.event,
      onDidChangeVisibleTextEditors: emptyEvent,
      onDidChangeTextEditorSelection: emptyEvent,
      tabGroups: { all: [group], activeTabGroup: group, onDidChangeTabs: emptyEvent, onDidChangeTabGroups: emptyEvent },
      createStatusBarItem: () => ({ show() {}, hide() {}, dispose() {} }),
      createTreeView: () => ({
        visible: false, selection: [],
        onDidChangeVisibility: emptyEvent, onDidChangeSelection: emptyEvent,
        onDidExpandElement: emptyEvent, onDidCollapseElement: emptyEvent,
        reveal: async () => {}, dispose() {}
      }),
      createTextEditorDecorationType: () => new Disposable(() => {}),
      showErrorMessage: alerts,
      showWarningMessage: alerts,
      showInformationMessage: alerts,
      withProgress: (_options, task) => task({ report() {} }, new CancellationTokenSource().token)
    });
    Object.defineProperties(api.window, {
      activeTextEditor: { get: () => session.editor },
      visibleTextEditors: { get: () => [...session.documents.values()].map(document => ({ document, viewColumn: 1 })) }
    });
    Object.assign(api.workspace, {
      isTrusted: true,
      notebookDocuments: [],
      onDidOpenNotebookDocument: emptyEvent,
      onDidCloseNotebookDocument: emptyEvent,
      onDidChangeNotebookDocument: emptyEvent,
      onDidSaveNotebookDocument: emptyEvent,
      onDidOpenTextDocument: this.open.event,
      onDidChangeTextDocument: this.change.event,
      onDidCloseTextDocument: this.close.event,
      onDidSaveTextDocument: this.save.event,
      onDidChangeConfiguration: emptyEvent,
      onDidChangeWorkspaceFolders: emptyEvent,
      onDidCreateFiles: emptyEvent,
      onDidDeleteFiles: emptyEvent,
      onDidRenameFiles: emptyEvent,
      onWillCreateFiles: emptyEvent,
      onWillDeleteFiles: emptyEvent,
      onWillRenameFiles: emptyEvent,
      onWillSaveTextDocument: emptyEvent,
      getConfiguration: section => getConfiguration(session.configuration ?? {}, section, session.configurationOverrides),
      getWorkspaceFolder: uri => session.folders.find(folder => {
        if (uri.scheme !== folder.uri.scheme) return false;
        const path = relative(folder.uri.fsPath, uri.fsPath);
        return path !== '..' && !path.startsWith('..' + sep) && !isAbsolute(path);
      }),
      createFileSystemWatcher: (...args) => session.watcher(...args),
      registerTextDocumentContentProvider(scheme, provider) {
        session.contentProviders.set(scheme, provider);
        return new Disposable(() => session.contentProviders.delete(scheme));
      },
      openTextDocument: async value => {
        const uri = typeof value === 'string' ? Uri.file(value) : value;
        const existing = session.documents.get(uri.toString());
        if (existing) return existing;
        const provider = session.contentProviders.get(uri.scheme);
        const text = provider
          ? await provider.provideTextDocumentContent(uri, new CancellationTokenSource().token)
          : await fs.readFile(uri.fsPath, 'utf8');
        return new TextDocument({ fileName: uri.fsPath, uri, text, version: 1, languageId: 'plaintext' });
      },
      fs: {
        readFile: uri => fs.readFile(fileURLToPath(uri.toString())),
        writeFile: (uri, data) => fs.writeFile(fileURLToPath(uri.toString()), data),
        createDirectory: uri => fs.mkdir(fileURLToPath(uri.toString()), { recursive: true }),
        readDirectory: async uri => (await fs.readdir(fileURLToPath(uri.toString()), { withFileTypes: true }))
          .map(entry => [entry.name, entry.isDirectory() ? 2 : entry.isSymbolicLink() ? 64 : 1]),
        stat: async uri => {
          const info = await fs.stat(fileURLToPath(uri.toString()));
          return { type: info.isDirectory() ? 2 : 1, ctime: info.ctimeMs, mtime: info.mtimeMs, size: info.size };
        }
      }
    });
    Object.defineProperties(api.workspace, {
      workspaceFolders: { get: () => session.folders },
      rootPath: { get: () => session.folders[0]?.uri.fsPath },
      textDocuments: { get: () => [...session.documents.values()] }
    });
    api.extensions = {
      get all() { return session.extension ? [session.extension] : []; },
      getExtension: id => session.extension?.id.toLowerCase() === id.toLowerCase() ? session.extension : undefined,
      onDidChange: emptyEvent
    };
    api.tasks = { registerTaskProvider: () => new Disposable(() => {}) };
    api.env = {
      appName: 'DotDir', appHost: 'desktop', uriScheme: 'dotdir', language: 'en', uiKind: 1,
      isTelemetryEnabled: false, onDidChangeTelemetryEnabled: emptyEvent,
      createTelemetryLogger: () => ({
        isUsageEnabled: false, isErrorsEnabled: false, onDidChangeEnableStates: emptyEvent,
        logUsage() {}, logError() {}, dispose() {}
      })
    };
    api.languages = new Proxy({
      match: (selector, document) => Number(documentMatches(selector, document)),
      createDiagnosticCollection: name => this.diagnostics(name),
      setLanguageConfiguration: () => new Disposable(() => {}),
      createLanguageStatusItem: id => ({ id, dispose() {} })
    }, {
      get: (target, key) => {
        if (Object.hasOwn(target, key)) return target[key];
        if (typeof key === 'string' && /^register[A-Z]\w*Provider$/.test(key)) {
          return (selector, provider) => this.register(key.slice(8, -8), selector, provider);
        }
      }
    });
    Object.assign(api, {
      version: '1.105.0',
      CodeActionKind: class CodeActionKind {
        constructor(value) { this.value = value; }
        append(value) { return new this.constructor([this.value, value].filter(Boolean).join('.')); }
        contains(kind) { return !this.value || kind.value === this.value || kind.value.startsWith(this.value + '.'); }
        intersects(kind) { return this.contains(kind) || kind.contains(this); }
      },
      ThemeIcon: class ThemeIcon { constructor(id, color) { Object.assign(this, { id, color }); } },
      ThemeColor: class ThemeColor { constructor(id) { this.id = id; } },
      TreeItem: class TreeItem {
        constructor(value, collapsibleState = 0) {
          if (typeof value === 'string') this.label = value;
          else this.resourceUri = value;
          this.collapsibleState = collapsibleState;
        }
      },
      TreeItemCollapsibleState: enumValues(['None', 'Collapsed', 'Expanded']),
      Uri, Disposable, EventEmitter, CancellationTokenSource, MarkdownString, TabInputText,
      CancellationError: class CancellationError extends Error {},
      Diagnostic: class Diagnostic { constructor(range, message, severity = 0) { Object.assign(this, { range, message, severity }); } },
      DiagnosticRelatedInformation: class DiagnosticRelatedInformation { constructor(location, message) { Object.assign(this, { location, message }); } },
      CompletionItem: class CompletionItem { constructor(label, kind) { Object.assign(this, { label, kind }); } },
      CompletionList: class CompletionList { constructor(items = [], isIncomplete = false) { Object.assign(this, { items, isIncomplete }); } },
      SnippetString: class SnippetString { constructor(value = '') { this.value = value; } },
      Hover: class Hover { constructor(contents, range) { Object.assign(this, { contents, range }); } },
      Location: class Location { constructor(uri, range) { Object.assign(this, { uri, range: range instanceof Position ? new Range(range, range) : range }); } },
      RelativePattern: class RelativePattern {
        constructor(base, pattern) {
          this.baseUri = typeof base === 'string' ? Uri.file(base) : base.uri ?? base;
          this.base = this.baseUri.fsPath;
          this.pattern = pattern;
        }
      },
      DiagnosticSeverity: enumValues(['Error', 'Warning', 'Information', 'Hint']),
      DiagnosticTag: enumValues(['Unnecessary', 'Deprecated'], 1),
      CompletionTriggerKind: enumValues(['Invoke', 'TriggerCharacter', 'TriggerForIncompleteCompletions']),
      CompletionItemKind: enumValues(['Text','Method','Function','Constructor','Field','Variable','Class','Interface','Module','Property','Unit','Value','Enum','Keyword','Snippet','Color','File','Reference','Folder','EnumMember','Constant','Struct','Event','Operator','TypeParameter']),
      CompletionItemTag: enumValues(['Deprecated'], 1),
      FileType: { Unknown: 0, File: 1, Directory: 2, SymbolicLink: 64 },
      FileChangeType: { Changed: 1, Created: 2, Deleted: 3 },
      ExtensionMode: { Production: 1, Development: 2, Test: 3 },
      UIKind: { Desktop: 1, Web: 2 },
      LogLevel: { Off: 0, Trace: 1, Debug: 2, Info: 3, Warning: 4, Error: 5 },
      StatusBarAlignment: { Left: 1, Right: 2 },
      LanguageStatusSeverity: enumValues(['Information', 'Warning', 'Error']),
      IndentAction: enumValues(['None', 'Indent', 'IndentOutdent', 'Outdent']),
      TextDocumentChangeReason: enumValues(['Undo', 'Redo'], 1),
      ProgressLocation: { SourceControl: 1, Window: 10, Notification: 15 },
      ConfigurationTarget: { Global: 1, Workspace: 2, WorkspaceFolder: 3 },
      l10n: { t: (message, ...args) => (typeof message === 'string' ? message : message.message)
        .replace(/\{(\d+)\}/g, (_, index) => args[Number(index)] ?? '') }
    });
    for (const [name, value] of Object.entries({
      Empty: '', QuickFix: 'quickfix', Refactor: 'refactor', RefactorExtract: 'refactor.extract',
      RefactorInline: 'refactor.inline', RefactorRewrite: 'refactor.rewrite', Source: 'source',
      SourceOrganizeImports: 'source.organizeImports', SourceFixAll: 'source.fixAll', Notebook: 'notebook'
    })) api.CodeActionKind[name] = new api.CodeActionKind(value);
    // These value types are subclassed when language clients load. Providers
    // for their corresponding features are registered independently here.
    for (const name of ['CallHierarchyItem', 'CodeAction', 'CodeLens', 'DocumentLink', 'InlayHint', 'SymbolInformation', 'TypeHierarchyItem']) {
      api[name] = class {};
    }
  }

  context(request) {
    const hash = value => createHash('sha256').update(value).digest('hex');
    const storage = resolve(request.storage, hash(request.extension));
    const root = this.folders[0]?.uri.fsPath ?? '';
    const workspace = resolve(storage, 'workspace', hash(root));
    const pkg = JSON.parse(readFileSync(resolve(request.root, 'package.json'), 'utf8'));
    this.extension = {
      id: request.extension, extensionUri: Uri.file(request.root), extensionPath: request.root,
      packageJSON: pkg, isActive: true,
      get exports() { return undefined; }
    };
    return {
      extension: this.extension, extensionUri: this.extension.extensionUri, extensionMode: 1,
      globalStorageUri: Uri.file(storage), storageUri: Uri.file(workspace),
      logUri: Uri.file(resolve(storage, 'logs')),
      globalState: memento(storage, 'state.json'), workspaceState: memento(workspace, 'state.json'),
      environmentVariableCollection: { replace() {}, append() {}, prepend() {}, delete() {}, clear() {} }
    };
  }

  register(kind, selector, provider) {
    const providers = this.providers.get(kind) ?? new Set();
    this.providers.set(kind, providers);
    const item = { selector, provider };
    providers.add(item);
    return new Disposable(() => providers.delete(item));
  }

  diagnostics(name) {
    const values = new Map();
    this.diagnosticCollections.add(values);
    const publish = (uri, diagnostics = []) => {
      const key = uri.toString();
      values.set(key, diagnostics);
      this.publish(key);
    };
    const clear = () => {
      const uris = [...values.keys()];
      values.clear();
      for (const uri of uris) this.publish(uri);
    };
    return {
      name,
      set: (uri, diagnostics) => Array.isArray(uri) ? uri.forEach(([key, items]) => publish(key, items)) : publish(uri, diagnostics),
      get: uri => values.get(uri.toString()),
      has: uri => values.has(uri.toString()),
      delete: uri => { values.delete(uri.toString()); this.publish(uri.toString()); },
      clear,
      dispose: () => { this.diagnosticCollections.delete(values); clear(); }
    };
  }

  publish(uri) {
    const document = this.documents.get(uri);
    if (!document) return;
    const diagnostics = [...this.diagnosticCollections].flatMap(items => items.get(uri) ?? [])
      .map(({ range, message, severity, code, source, tags, relatedInformation }) => ({
        range, message, severity: severity === undefined ? undefined : severity + 1,
        code: typeof code === 'object' ? code?.value : code, source, tags,
        relatedInformation: relatedInformation?.map(item => ({
          message: item.message, location: { uri: item.location.uri.toString(), range: item.location.range }
        }))
      }));
    this.send({ method: 'textDocument/publishDiagnostics', params: { uri, version: document.version, diagnostics } });
  }

  watcher(pattern, ignoreCreate, ignoreChange, ignoreDelete) {
    const create = new EventEmitter(), change = new EventEmitter(), remove = new EventEmitter();
    const roots = typeof pattern === 'string' ? this.folders.map(folder => folder.uri.fsPath) : [pattern.baseUri?.fsPath ?? pattern.base];
    const entries = new Map();
    const watchers = roots.flatMap(root => {
      if (!root) return [];
      try {
        const watcher = watch(root, { recursive: true, persistent: false }, (event, name) => {
          if (!name) return;
          const path = resolve(root, name);
          if (!pathMatches(typeof pattern === 'string' ? { base: root, pattern } : pattern, path)) return;
          const uri = Uri.file(path);
          if (event === 'change' || entries.get(path) && existsSync(path)) {
            entries.set(path, true);
            if (!ignoreChange) change.fire(uri);
          } else if (existsSync(path)) {
            entries.set(path, true);
            if (!ignoreCreate) create.fire(uri);
          } else {
            entries.delete(path);
            if (!ignoreDelete) remove.fire(uri);
          }
        });
        watcher.on('error', error => console.error(error));
        this.readers.add(watcher);
        return [watcher];
      } catch (error) { console.error(error); return []; }
    });
    return {
      ignoreCreateEvents: !!ignoreCreate, ignoreChangeEvents: !!ignoreChange, ignoreDeleteEvents: !!ignoreDelete,
      onDidCreate: create.event, onDidChange: change.event, onDidDelete: remove.event,
      dispose: () => {
        for (const watcher of watchers) { watcher.close(); this.readers.delete(watcher); }
        create.dispose(); change.dispose(); remove.dispose();
      }
    };
  }

  send(message) {
    if (this.disconnected) return;
    const body = JSON.stringify({ jsonrpc: '2.0', ...message });
    protocol('Content-Length: ' + Buffer.byteLength(body) + '\r\n\r\n' + body);
  }

  stop() {
    return this.stopping ??= Promise.resolve().then(async () => {
      for (const watcher of this.readers) watcher.close();
      this.readers.clear();
      await this.deactivate?.();
    });
  }

  start(activateExtension) {
    let bytes = Buffer.alloc(0);
    let size;
    let queue = Promise.resolve();
    process.stdin.on('data', chunk => {
      bytes = Buffer.concat([bytes, chunk]);
      while (true) {
        if (size === undefined) {
          const end = bytes.indexOf('\r\n\r\n');
          if (end < 0) {
            if (bytes.length > 8192) process.exit(1);
            return;
          }
          size = Number(/Content-Length:\s*(\d+)/i.exec(bytes.subarray(0, end).toString())?.[1]);
          if (!Number.isSafeInteger(size) || size > 128 * 1024 * 1024) { process.exit(1); }
          bytes = bytes.subarray(end + 4);
        }
        if (bytes.length < size) return;
        const message = JSON.parse(bytes.subarray(0, size).toString());
        bytes = bytes.subarray(size);
        size = undefined;
        if (message.method === '$/cancelRequest') {
          this.requests.get(message.params.id)?.cancel();
        } else if (message.method === 'exit') {
          process.exit();
        } else {
          queue = queue.then(() => this.dispatch(message, activateExtension)).catch(error => console.error(error));
        }
      }
    });
    const disconnect = () => {
      if (this.disconnected) return;
      this.disconnected = true;
      setTimeout(() => process.exit(), 700).unref();
      void this.stop().catch(error => console.error(error)).finally(() => process.exit());
    };
    process.stdin.on('end', disconnect);
    process.stdout.on('error', disconnect);
  }

  async dispatch({ id, method, params }, activateExtension) {
    const token = id === undefined ? undefined : new CancellationTokenSource();
    if (token) this.requests.set(id, token);
    try {
      const result = await this.handle(method, params, token?.token, activateExtension);
      if (id !== undefined) this.send({ id, result: result ?? null });
    } catch (error) {
      if (id !== undefined) this.send({ id, error: { code: -32603, message: String(error?.message ?? error) } });
      else this.send({ method: 'window/logMessage', params: { type: 1, message: String(error?.stack ?? error) } });
    } finally {
      if (token) { this.requests.delete(id); token.dispose(); }
    }
  }

  async handle(method, params, token, activateExtension) {
    if (method === 'initialize') {
      this.request = params.initializationOptions;
      this.configuration = this.request.configuration;
      this.configurationOverrides = this.request.configurationOverrides;
      this.folders = (params.workspaceFolders ?? []).map((folder, index) => ({ uri: Uri.parse(folder.uri), name: folder.name, index }));
      return { capabilities: {
        textDocumentSync: { openClose: true, change: 2, save: { includeText: false } },
        hoverProvider: true, completionProvider: {}, definitionProvider: true
      } };
    }
    if (method === 'shutdown') {
      await this.stop();
      return;
    }
    if (method === 'textDocument/didOpen') {
      const item = params.textDocument;
      const uri = Uri.parse(item.uri);
      const document = new TextDocument({ ...item, uri, fileName: uri.fsPath });
      this.documents.set(item.uri, document);
      this.editor = { document, selection: new Selection(0, 0, 0, 0), viewColumn: 1 };
      if (!this.activating) {
        this.activating = Promise.resolve().then(() => activateExtension(this.request, this.context(this.request)));
        try {
          await this.activating;
        } catch (error) {
          console.error(error);
          this.send({ method: 'dotdir/activationFailed', params: { message: String(error?.message ?? error) } });
          return;
        }
      } else {
        this.open.fire(document);
      }
      this.active.fire(this.editor);
      this.send({ method: 'dotdir/documentReady', params: { uri: item.uri } });
      return;
    }
    if (method === 'textDocument/didChange') {
      const document = this.documents.get(params.textDocument.uri);
      if (!document) return;
      const contentChanges = params.contentChanges.map(({ range, text }) => {
        const start = range ? document.offsetAt(range.start) : 0;
        const end = range ? document.offsetAt(range.end) : document.text.length;
        const change = { text, range: range && makeRange(range), rangeOffset: start, rangeLength: end - start };
        document.update(document.text.slice(0, start) + text + document.text.slice(end), params.textDocument.version);
        return change;
      });
      document.isDirty = true;
      this.change.fire({ document, contentChanges });
      return;
    }
    if (method === 'textDocument/didSave') {
      const document = this.documents.get(params.textDocument.uri);
      if (document) { document.isDirty = false; this.save.fire(document); }
      return;
    }
    if (method === 'textDocument/didClose') {
      const document = this.documents.get(params.textDocument.uri);
      this.documents.delete(params.textDocument.uri);
      if (document) { document.isClosed = true; this.close.fire(document); }
      if (document && this.editor?.document === document) {
        const next = [...this.documents.values()].at(-1);
        this.editor = next && { document: next, selection: new Selection(0, 0, 0, 0), viewColumn: 1 };
        this.active.fire(this.editor);
      }
      return;
    }
    const selected = providerKinds.get(method);
    if (!selected) return;
    await this.activating;
    const document = this.documents.get(params.textDocument.uri);
    if (!document) return;
    const [kind, action] = selected;
    const position = new Position(params.position.line, params.position.character);
    for (const { selector, provider } of this.providers.get(kind) ?? []) {
      if (!documentMatches(selector, document)) continue;
      const context = { triggerKind: (params.context?.triggerKind ?? 1) - 1, triggerCharacter: params.context?.triggerCharacter };
      const result = await provider[action](document, position, token, context);
      if (result != null) return languageResult(kind, result);
    }
  }
}

function makeRange({ start, end }) {
  return new Range(start.line, start.character, end.line, end.character);
}

function languageResult(kind, result) {
  if (kind === 'Hover') {
    const contents = Array.isArray(result.contents) ? result.contents : [result.contents];
    return { contents: { kind: 'markdown', value: contents.map(item => {
      if (typeof item === 'string') return item;
      return item.language ? '\n```' + item.language + '\n' + item.value + '\n```\n' : item.value;
    }).join('\n\n') }, range: result.range };
  }
  if (kind === 'Definition') {
    const location = item => item.targetUri
      ? { ...item, targetUri: item.targetUri.toString() }
      : { uri: item.uri.toString(), range: item.range };
    return Array.isArray(result) ? result.map(location) : location(result);
  }
  const items = Array.isArray(result) ? result : result.items;
  return {
    isIncomplete: !!result.isIncomplete,
    items: items.map(item => {
      const label = typeof item.label === 'string' ? item.label : item.label.label;
      const snippet = typeof item.insertText?.value === 'string';
      const insertText = snippet ? item.insertText.value : item.insertText;
      const replacement = insertText ?? label;
      const textEdit = item.range?.inserting
        ? { insert: item.range.inserting, replace: item.range.replacing, newText: replacement }
        : item.range && { range: item.range, newText: replacement };
      return {
        label, kind: item.kind === undefined ? undefined : item.kind + 1, detail: item.detail,
        documentation: typeof item.documentation === 'object'
          ? { kind: 'markdown', value: item.documentation.value } : item.documentation,
        insertText, insertTextFormat: snippet ? 2 : 1, sortText: item.sortText,
        filterText: item.filterText, preselect: item.preselect,
        textEdit: item.textEdit ?? textEdit,
        additionalTextEdits: item.additionalTextEdits, commitCharacters: item.commitCharacters
      };
    })
  };
}
