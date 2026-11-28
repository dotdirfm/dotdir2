import { existsSync, mkdirSync, renameSync, rmSync, watch, writeFileSync } from 'node:fs';
import * as fs from 'node:fs/promises';
import { matchesGlob } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';

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

function getConfiguration(section, defaults, overrides, workspace, update) {
  const values = { ...defaults, ...overrides };
  for (const key of workspace.keys()) {
    Object.defineProperty(values, key, { value: workspace.get(key), enumerable: true, configurable: true });
  }
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
    inspect: { value: key => {
      const name = keyFor(key);
      return {
        key: name,
        defaultValue: Object.hasOwn(defaults, name) ? defaults[name] : undefined,
        globalValue: Object.hasOwn(overrides, name) ? overrides[name] : undefined,
        workspaceValue: workspace.get(name)
      };
    } },
    update: { value: (key, value, target) => update(keyFor(key), value, target) }
  });
  return result;
}

function memento(directory, filename) {
  const path = resolve(directory, filename);
  const read = () => {
    try { return JSON.parse(readFileSync(path, 'utf8')); } catch { return {}; }
  };
  let data = read();
  return {
    get: (key, fallback) => Object.hasOwn(data, key) ? data[key] : fallback,
    keys: () => Object.keys(data),
    async update(key, value) {
      const updated = read();
      if (value === undefined) delete updated[key];
      else Object.defineProperty(updated, key, { value, enumerable: true, configurable: true, writable: true });
      mkdirSync(directory, { recursive: true });
      const temporary = resolve(directory, filename + '.' + randomUUID());
      try {
        writeFileSync(temporary, JSON.stringify(updated), { mode: 0o600 });
        renameSync(temporary, path);
      } finally {
        rmSync(temporary, { force: true });
      }
      data = updated;
    },
    setKeysForSync() {}
  };
}

class TextEdit {
  constructor(range, newText) { Object.assign(this, { range, newText }); }
  static replace(range, text) { return new TextEdit(range, text); }
  static insert(position, text) { return TextEdit.replace(new Range(position, position), text); }
  static delete(range) { return TextEdit.replace(range, ''); }
}

class WorkspaceEdit {
  documents = new Map();
  hasResourceChanges = false;
  set(uri, edits) {
    const key = uri.toString();
    this.documents.set(key, [uri, [...(this.documents.get(key)?.[1] ?? []), ...edits]]);
  }
  entries() { return [...this.documents.values()]; }
  replace(uri, range, text) { this.set(uri, [TextEdit.replace(range, text)]); }
  insert(uri, position, text) { this.set(uri, [TextEdit.insert(position, text)]); }
  delete(uri, range) { this.set(uri, [TextEdit.delete(range)]); }
  createFile() { this.hasResourceChanges = true; }
  renameFile() { this.hasResourceChanges = true; }
  deleteFile() { this.hasResourceChanges = true; }
}

function bufferEdits(edit, uri) {
  if (edit.hasResourceChanges) return;
  const entries = edit.entries();
  if (entries.length !== 1 || entries[0][0].toString() !== uri) return;
  const changes = [];
  for (const entry of entries[0][1]) {
    const [item, metadata] = Array.isArray(entry) ? entry : [entry];
    if (metadata?.needsConfirmation || !item?.range || typeof item.newText !== 'string') return;
    changes.push({ range: item.range, newText: item.newText });
  }
  if (changes.length) return { changes: { [uri]: changes } };
}

class LanguageSession {
  documents = new Map();
  tabs = new Map();
  providers = new Map();
  open = new EventEmitter();
  change = new EventEmitter();
  close = new EventEmitter();
  save = new EventEmitter();
  active = new EventEmitter();
  tabsChanged = new EventEmitter();
  visibleEditorsChanged = new EventEmitter();
  configurationChanged = new EventEmitter();
  contentProviders = new Map();
  readers = new Set();
  diagnosticCollections = new Set();
  folders = [];
  requests = new Map();
  codeActions = new Map();
  nextCodeAction = 0;
  nextQuickPick = 0;

  constructor(api) {
    this.commands = api.commands;
    const session = this;
    const group = {
      get activeTab() {
        return session.editor && session.tabs.get(session.editor.document.uri.toString());
      },
      get tabs() { return [...session.tabs.values()]; },
      isActive: true,
      viewColumn: 1
    };
    this.tabGroup = group;
    const alerts = message => { console.error(message); return Promise.resolve(undefined); };
    Object.assign(api.window, {
      onDidChangeActiveTextEditor: this.active.event,
      onDidChangeVisibleTextEditors: this.visibleEditorsChanged.event,
      onDidChangeTextEditorSelection: emptyEvent,
      tabGroups: { all: [group], activeTabGroup: group, onDidChangeTabs: this.tabsChanged.event, onDidChangeTabGroups: emptyEvent },
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
      showQuickPick: (items, options, token) => this.showQuickPick(items, options, token),
      withProgress: (_options, task) => task({ report() {} }, new CancellationTokenSource().token)
    });
    Object.defineProperties(api.window, {
      activeTextEditor: { get: () => session.editor },
      visibleTextEditors: { get: () => session.visibleEditors() }
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
      onDidChangeConfiguration: this.configurationChanged.event,
      onDidChangeWorkspaceFolders: emptyEvent,
      onDidCreateFiles: emptyEvent,
      onDidDeleteFiles: emptyEvent,
      onDidRenameFiles: emptyEvent,
      onWillCreateFiles: emptyEvent,
      onWillDeleteFiles: emptyEvent,
      onWillRenameFiles: emptyEvent,
      onWillSaveTextDocument: emptyEvent,
      getConfiguration: section => getConfiguration(
        section, session.configuration, session.configurationOverrides, session.workspaceConfiguration,
        (key, value, target) => session.updateConfiguration(key, value, target)
      ),
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
    Object.assign(api.env, {
      appName: 'DotDir', appHost: 'desktop', uriScheme: 'dotdir', uiKind: 1,
      isTelemetryEnabled: false, onDidChangeTelemetryEnabled: emptyEvent,
      createTelemetryLogger: () => ({
        isUsageEnabled: false, isErrorsEnabled: false, onDidChangeEnableStates: emptyEvent,
        logUsage() {}, logError() {}, dispose() {}
      })
    });
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
      Disposable, EventEmitter, CancellationTokenSource, MarkdownString, TabInputText,
      CancellationError: class CancellationError extends Error {},
      Diagnostic: class Diagnostic { constructor(range, message, severity = 0) { Object.assign(this, { range, message, severity }); } },
      DiagnosticRelatedInformation: class DiagnosticRelatedInformation { constructor(location, message) { Object.assign(this, { location, message }); } },
      TextEdit, WorkspaceEdit,
      CodeAction: class CodeAction { constructor(title, kind) { Object.assign(this, { title, kind }); } },
      CodeActionTriggerKind: enumValues(['Invoke', 'Automatic'], 1),
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
      QuickPickItemKind: { Separator: -1, Default: 0 }
    });
    for (const [name, value] of Object.entries({
      Empty: '', QuickFix: 'quickfix', Refactor: 'refactor', RefactorExtract: 'refactor.extract',
      RefactorInline: 'refactor.inline', RefactorRewrite: 'refactor.rewrite', Source: 'source',
      SourceOrganizeImports: 'source.organizeImports', SourceFixAll: 'source.fixAll', Notebook: 'notebook'
    })) api.CodeActionKind[name] = new api.CodeActionKind(value);
    // These value types are subclassed when language clients load. Providers
    // for their corresponding features are registered independently here.
    for (const name of ['CallHierarchyItem', 'CodeLens', 'DocumentLink', 'InlayHint', 'SymbolInformation', 'TypeHierarchyItem']) {
      api[name] = class {};
    }
  }

  context(request) {
    const hash = value => createHash('sha256').update(value).digest('hex');
    const storage = resolve(request.storage, hash(request.extension));
    const root = this.folders[0]?.uri.fsPath;
    const workspace = resolve(storage, 'workspace', hash(root ?? ''));
    const configuration = resolve(storage, 'workspace', hash(root ?? process.cwd()));
    this.workspaceConfiguration = memento(configuration, 'configuration.json');
    const pkg = request.localization?.packageJson ?? JSON.parse(readFileSync(resolve(request.root, 'package.json'), 'utf8'));
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

  async updateConfiguration(key, value, target) {
    if (target !== 2 && target !== false) {
      throw new Error('DotDir supports workspace-scoped configuration updates');
    }
    const previous = this.workspaceConfiguration.get(key);
    if (isDeepStrictEqual(previous, value)) return;
    const inherited = Object.hasOwn(this.configurationOverrides, key)
      ? this.configurationOverrides[key]
      : this.configuration[key];
    await this.workspaceConfiguration.update(key, value);
    if (isDeepStrictEqual(previous === undefined ? inherited : previous, value === undefined ? inherited : value)) return;
    this.configurationChanged.fire({
      affectsConfiguration: section => section === key || key.startsWith(section + '.') || section.startsWith(key + '.')
    });
  }

  async showQuickPick(items, options = {}, token) {
    const command = this.commandRequest;
    items = await items;
    if (token?.isCancellationRequested || !items.length) return;
    if (command === undefined || command !== this.commandRequest) {
      throw new Error('DotDir quick picks require an active extension command');
    }
    if (options.canPickMany) throw new Error('DotDir quick picks support single selection');
    if (this.quickPickReply) throw new Error('Another quick pick is already open');
    const id = 'quick-pick:' + (++this.nextQuickPick);
    const selected = new Promise(finish => { this.quickPickReply = { id, finish }; });
    const cancel = token?.onCancellationRequested(() => {
      this.send({ method: 'dotdir/cancelQuickPick', params: { id } });
    });
    try {
      const rows = items.map(item => typeof item === 'string' ? { label: item } : {
        label: item.label, description: item.description, detail: item.detail, separator: item.kind === -1
      });
      this.send({ id, method: 'dotdir/showQuickPick', params: {
        command: command.id, items: rows, title: options.title, placeHolder: options.placeHolder,
        matchOnDescription: !!options.matchOnDescription, matchOnDetail: !!options.matchOnDetail
      } });
      const index = await selected;
      if (!token?.isCancellationRequested && Number.isInteger(index) && !rows[index]?.separator) {
        return items[index];
      }
    } finally {
      cancel?.dispose();
      this.quickPickReply = undefined;
    }
  }

  async goToLocations(_uri, _position, locations, multiple, noResultsMessage) {
    const command = this.commandRequest;
    if (!command) throw new Error('DotDir location navigation requires an active extension command');
    if (!locations.length) {
      command.result = { type: 'status', message: noResultsMessage ?? 'No locations found.' };
      return;
    }
    const local = locations.filter(location => location.uri?.scheme === 'file');
    if (!local.length) throw new Error('DotDir source navigation requires a file URI');
    let location = local[0];
    if (local.length > 1 && multiple !== 'goto') {
      const items = local.map(location => ({
        label: basename(location.uri.fsPath),
        description: String(location.range.start.line + 1),
        detail: location.uri.fsPath,
        location
      }));
      const choice = await this.showQuickPick(items, { placeHolder: 'Select a source location', matchOnDetail: true });
      if (!choice) return;
      location = choice.location;
    }
    command.result = {
      type: 'goToLocation',
      location: { uri: location.uri.toString(), range: location.range }
    };
  }

  visibleEditors() {
    return [...this.documents.values()].map(document => ({ document, viewColumn: 1 }));
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

  async listCodeActions(params, token) {
    const uri = params.textDocument.uri;
    const document = this.documents.get(uri);
    if (!document || document.version !== params.version) return [];
    const range = makeRange(params.range);
    const diagnostics = [...this.diagnosticCollections]
      .flatMap(items => items.get(uri) ?? [])
      .filter(item => !item.range.end.isBefore(range.start) && !item.range.start.isAfter(range.end));
    const unresolved = new Map();
    const result = [];
    for (const { selector, provider } of this.providers.get('CodeActions') ?? []) {
      if (!documentMatches(selector, document)) continue;
      const items = await provider.provideCodeActions(document, range, { diagnostics, triggerKind: 1 }, token);
      if (token.isCancellationRequested || document.version !== params.version) return [];
      for (const action of items ?? []) {
        if (!action.title || action.disabled || action.command) continue;
        const item = { title: action.title, kind: action.kind?.value, isPreferred: action.isPreferred };
        if (action.edit) {
          item.edit = bufferEdits(action.edit, uri);
          if (!item.edit) continue;
        } else if (provider.resolveCodeAction) {
          const id = ++this.nextCodeAction;
          unresolved.set(id, { action, provider });
          item.data = { uri, version: params.version, id };
        } else {
          continue;
        }
        result.push(item);
      }
    }
    this.codeActions.set(uri, unresolved);
    return result;
  }

  async resolveCodeAction(item, token) {
    const { uri, version, id } = item.data;
    const document = this.documents.get(uri);
    const pending = this.codeActions.get(uri)?.get(id);
    if (!pending || document?.version !== version) throw new Error('Code action expired');
    const action = await pending.provider.resolveCodeAction(pending.action, token);
    if (document.version !== version) throw new Error('Code action expired');
    const edit = action?.edit && bufferEdits(action.edit, uri);
    if (!edit || action.command || action.disabled) throw new Error('Code action requires changes outside this editor');
    return { ...item, edit };
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
        if (message.method === undefined) {
          if (this.quickPickReply && message.id === this.quickPickReply.id) this.quickPickReply.finish(message.result);
        } else if (message.method === '$/cancelRequest') {
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
    if (method === 'dotdir/executeCommand') this.commandRequest = { id };
    try {
      const result = await this.handle(method, params, token?.token, activateExtension);
      if (id !== undefined) this.send({ id, result: result ?? null });
    } catch (error) {
      if (id !== undefined) {
        const { message, stack } = errorDetails(error);
        this.send({ id, error: { code: -32603, message, data: { stack } } });
      }
      else this.send({ method: 'window/logMessage', params: { type: 1, message: String(error?.stack ?? error) } });
    } finally {
      if (method === 'dotdir/executeCommand') this.commandRequest = undefined;
      if (token) { this.requests.delete(id); token.dispose(); }
    }
  }

  publishCommands() {
    if (!this.activated || this.commandsQueued) return;
    this.commandsQueued = true;
    queueMicrotask(() => {
      this.commandsQueued = false;
      this.send({ method: 'dotdir/commandsChanged', params: { commands: this.registeredCommands() } });
    });
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
      const session = this;
      const previousTab = this.tabGroup.activeTab;
      const tab = {
        input: new TabInputText(uri),
        group: this.tabGroup,
        get isActive() { return session.editor?.document === document; },
        get isDirty() { return document.isDirty; }
      };
      this.documents.set(item.uri, document);
      this.tabs.set(item.uri, tab);
      this.editor = { document, selection: new Selection(0, 0, 0, 0), viewColumn: 1 };
      if (!this.activating) {
        this.activating = Promise.resolve().then(() => activateExtension(this.request, this.context(this.request)));
        try {
          await this.activating;
          this.activated = true;
          this.publishCommands();
        } catch (error) {
          this.send({ method: 'dotdir/activationFailed', params: errorDetails(error) });
          return;
        }
      } else {
        this.open.fire(document);
      }
      this.tabsChanged.fire({ opened: [tab], closed: [], changed: previousTab ? [previousTab] : [] });
      this.visibleEditorsChanged.fire(this.visibleEditors());
      this.active.fire(this.editor);
      this.send({ method: 'dotdir/documentReady', params: { uri: item.uri } });
      return;
    }
    if (method === 'textDocument/didChange') {
      const document = this.documents.get(params.textDocument.uri);
      if (!document) return;
      this.codeActions.delete(params.textDocument.uri);
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
      const { uri } = params.textDocument;
      const document = this.documents.get(uri);
      const tab = this.tabs.get(uri);
      if (!document || !tab) return;
      this.documents.delete(uri);
      this.tabs.delete(uri);
      this.codeActions.delete(uri);
      document.isClosed = true;
      this.close.fire(document);
      const activeChanged = this.editor?.document === document;
      if (activeChanged) {
        const next = [...this.documents.values()].at(-1);
        this.editor = next && { document: next, selection: new Selection(0, 0, 0, 0), viewColumn: 1 };
      }
      const activeTab = this.tabGroup.activeTab;
      this.tabsChanged.fire({ opened: [], closed: [tab], changed: activeChanged && activeTab ? [activeTab] : [] });
      this.visibleEditorsChanged.fire(this.visibleEditors());
      if (activeChanged) this.active.fire(this.editor);
      return;
    }
    if (method === 'dotdir/executeCommand') {
      await this.activating;
      const document = this.documents.get(params.textDocument.uri);
      if (!document) throw new Error('The editor document is no longer open');
      const { anchor, active } = params;
      const selection = new Selection(anchor.line, anchor.character, active.line, active.character);
      const changed = this.editor?.document !== document;
      this.editor = { document, selection, viewColumn: 1 };
      if (changed) this.active.fire(this.editor);
      await this.commands.executeCommand(params.command);
      return this.commandRequest.result;
    }
    if (method === 'textDocument/codeAction') {
      await this.activating;
      return this.listCodeActions(params, token);
    }
    if (method === 'codeAction/resolve') return this.resolveCodeAction(params, token);
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
