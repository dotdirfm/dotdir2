import { readFileSync, realpathSync } from 'node:fs';
import { AsyncLocalStorage } from 'node:async_hooks';
import { createRequire, flushCompileCache, registerHooks } from 'node:module';
import { basename, isAbsolute, posix, relative, resolve, sep } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath, pathToFileURL } from 'node:url';

// Stdout carries protocol messages only, including when an extension logs.
const protocol = process.stdout.write.bind(process.stdout);
const send = (message) => protocol(JSON.stringify(message) + '\n');
const commandLog = new AsyncLocalStorage();

function errorDetails(error) {
  const message = String(error?.message ?? error);
  const stack = typeof error?.stack === 'string'
    ? error.stack.split('\n').slice(1).join('\n').slice(0, 8192)
    : undefined;
  return { message, stack };
}
if (process.env.DOTDIR_VSCODE_LANGUAGE !== '1') {
  const stderr = process.stderr.write.bind(process.stderr);
  process.stderr.write = (chunk, encoding, callback) => {
    const extension = commandLog.getStore();
    if (!extension) return stderr(chunk, encoding, callback);
    const buffer = typeof chunk === 'string'
      ? Buffer.from(chunk, typeof encoding === 'string' ? encoding : undefined)
      : Buffer.from(chunk);
    const written = send({ type: 'log', extension, message: buffer.subarray(0, 8192).toString() });
    const done = typeof encoding === 'function' ? encoding : callback;
    if (done) queueMicrotask(done);
    return written;
  };
}
process.stdout.write = process.stderr.write.bind(process.stderr);

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

function localization() {
  const extensionId = commandLog.getStore();
  const extension = extensionId && extensionId !== current?.extensionId
    ? extensions.get(extensionId)
    : current?.extension;
  return extension?.localization;
}

function translate(messageOrOptions, ...args) {
  const options = typeof messageOrOptions === 'string'
    ? { message: messageOrOptions, args: typeof args[0] === 'object' && args[0] !== null ? args[0] : args }
    : messageOrOptions;
  const { message, comment, args: values = {} } = options;
  const note = Array.isArray(comment) ? comment.join('') : comment;
  const key = note ? message + '/' + note : message;
  const messages = localization()?.bundle?.messages;
  const text = messages && Object.hasOwn(messages, key) ? messages[key] : message;
  return text.replace(/\{([^{}]+)\}/g, (placeholder, name) =>
    Object.hasOwn(values, name) ? String(values[name]) : placeholder);
}

function createOutputChannel(name) {
  const log = (message, ...args) => console.error('[' + name + ']', message, ...args);
  return {
    name, logLevel: 2, onDidChangeLogLevel: () => ({ dispose() {} }),
    append: log, appendLine: log, trace: log, debug: log, info: log, warn: log, error: log,
    clear() {}, replace: log, show() {}, hide() {}, dispose() {}
  };
}

class Position {
  constructor(line, character) {
    if (!Number.isSafeInteger(line) || line < 0 || !Number.isSafeInteger(character) || character < 0) {
      throw new RangeError('Invalid position');
    }
    this.line = line;
    this.character = character;
  }

  compareTo(other) {
    return Math.sign(this.line - other.line || this.character - other.character);
  }

  isEqual(other) { return this.compareTo(other) === 0; }
  isBefore(other) { return this.compareTo(other) < 0; }
  isBeforeOrEqual(other) { return this.compareTo(other) <= 0; }
  isAfter(other) { return this.compareTo(other) > 0; }
  isAfterOrEqual(other) { return this.compareTo(other) >= 0; }
  translate(line = 0, character = 0) {
    if (typeof line === 'object') ({ lineDelta: line = 0, characterDelta: character = 0 } = line);
    return new Position(this.line + line, this.character + character);
  }
  with(line = this.line, character = this.character) {
    if (typeof line === 'object') ({ line: line = this.line, character: character = this.character } = line);
    return new Position(line, character);
  }
}

class Range {
  constructor(start, end, endLine, endCharacter) {
    if (typeof start === 'number') {
      start = new Position(start, end);
      end = new Position(endLine, endCharacter);
    }
    [this.start, this.end] = start.isBefore(end) ? [start, end] : [end, start];
  }

  get isEmpty() { return this.start.isEqual(this.end); }
  get isSingleLine() { return this.start.line === this.end.line; }
  contains(value) {
    return value instanceof Range ? this.contains(value.start) && this.contains(value.end)
      : this.start.isBeforeOrEqual(value) && this.end.isAfterOrEqual(value);
  }
}

class Selection extends Range {
  constructor(anchor, active, activeLine, activeCharacter) {
    if (typeof anchor === 'number') {
      anchor = new Position(anchor, active);
      active = new Position(activeLine, activeCharacter);
    }
    super(anchor, active);
    this.anchor = anchor;
    this.active = active;
  }

  get isReversed() { return this.active.isBefore(this.anchor); }
}

class TextDocument {
  constructor(snapshot) {
    this.fileName = snapshot.fileName;
    this.languageId = snapshot.languageId;
    this.uri = snapshot.uri;
    this.isUntitled = false;
    this.isDirty = false;
    this.isClosed = false;
    this.update(snapshot.text, snapshot.version);
  }

  update(text, version) {
    this.version = version;
    this.text = text;
    this.eol = /\r?\n/.exec(this.text)?.[0] === '\r\n' ? 2 : 1;
    this.lines = this.text.split(/\r?\n/);
    this.offsets = [0];
    for (const newline of this.text.matchAll(/\n/g)) this.offsets.push(newline.index + 1);
  }

  get lineCount() { return this.lines.length; }

  offsetAt(position) {
    if (position.line >= this.lineCount) return this.text.length;
    return this.offsets[position.line] + Math.min(position.character, this.lines[position.line].length);
  }

  positionAt(offset) {
    const bounded = Math.max(0, Math.min(offset, this.text.length));
    let low = 0, high = this.offsets.length;
    while (low + 1 < high) {
      const middle = (low + high) >>> 1;
      if (this.offsets[middle] <= bounded) low = middle;
      else high = middle;
    }
    return new Position(low, Math.min(bounded - this.offsets[low], this.lines[low].length));
  }

  getText(range) {
    return range ? this.text.slice(this.offsetAt(range.start), this.offsetAt(range.end)) : this.text;
  }

  lineAt(line) {
    const number = typeof line === 'number' ? line : line.line;
    const text = this.lines[number];
    if (text === undefined) throw new RangeError('Invalid line number');
    return {
      lineNumber: number,
      text,
      range: new Range(number, 0, number, text.length),
      rangeIncludingLineBreak: number + 1 < this.lineCount
        ? new Range(number, 0, number + 1, 0)
        : new Range(number, 0, number, text.length),
      firstNonWhitespaceCharacterIndex: text.length - text.trimStart().length,
      isEmptyOrWhitespace: !text.trim()
    };
  }
}

const extensions = new Map();
let current;
let nextEdit = 0;

function editorState(snapshot) {
  return {
    document: new TextDocument(snapshot),
    selection: new Selection(
      new Position(snapshot.anchor.line, snapshot.anchor.character),
      new Position(snapshot.active.line, snapshot.active.character)
    )
  };
}

function textEditor(job, snapshot) {
  return {
    ...editorState(snapshot),
    get selections() { return [this.selection]; },
    async edit(callback) {
      if (current !== job) return false;
      const edits = [];
      const add = (range, text) => {
        if (typeof text !== 'string') throw new TypeError('Expected replacement text');
        const eol = this.document.eol === 2 ? '\r\n' : '\n';
        edits.push({ start: range.start, end: range.end, text: text.replace(/\r?\n/g, eol) });
      };
      callback({
        replace(range, text) {
          add(range instanceof Position ? new Range(range, range) : range, text);
        },
        insert(position, text) { add(new Range(position, position), text); },
        delete(range) { add(range, ''); }
      });
      if (!edits.length) return true;
      const serial = ++nextEdit;
      const ack = new Promise((done) => job.edits.set(serial, done));
      const applied = ack.then((updated) => {
        if (!updated) return false;
        Object.assign(this, editorState(updated));
        return true;
      });
      job.transactions.push(applied);
      send({ type: 'edit', id: job.id, serial, version: this.document.version, edits });
      return applied;
    }
  };
}

const api = {
  Uri,
  Position,
  Range,
  Selection,
  EndOfLine: Object.freeze({ LF: 1, CRLF: 2 }),
  env: { get language() { return localization()?.locale ?? 'en'; } },
  l10n: {
    t: translate,
    get bundle() { return localization()?.bundle?.messages; },
    get uri() {
      const path = localization()?.bundle?.path;
      return path && Uri.file(path);
    }
  },
  ViewColumn: { Active: -1, Beside: -2, One: 1, Two: 2, Three: 3, Four: 4, Five: 5, Six: 6, Seven: 7, Eight: 8, Nine: 9 },
  window: {
    createOutputChannel,
    createWebviewPanel() { throw new Error('This command requires VS Code webviews, which DotDir does not support yet'); },
    get activeTextEditor() { return current?.editor; }
  },
  workspace: {
    getConfiguration(section) {
      const values = current?.configuration ?? {};
      const key = (name) => section ? section + '.' + name : name;
      return {
        get(name, defaultValue) { return Object.hasOwn(values, key(name)) ? values[key(name)] : defaultValue; },
        has(name) { return Object.hasOwn(values, key(name)); }
      };
    }
  },
  commands: {
    registerCommand(id, callback, thisArg) {
      const commands = current?.extension.commands;
      if (!commands || commands.has(id) || typeof callback !== 'function') {
        throw new Error('Cannot register command ' + id);
      }
      const command = callback.bind(thisArg);
      commands.set(id, command);
      languageSession?.publishCommands();
      return { dispose() {
        if (commands.get(id) === command) {
          commands.delete(id);
          languageSession?.publishCommands();
        }
      } };
    },
    async executeCommand(id, ...args) {
      if (id === 'setContext') return;
      if (id === 'editor.action.goToLocations' && languageSession) {
        return await languageSession.goToLocations(...args);
      }
      const callback = current?.extension.commands.get(id);
      if (!callback) throw new Error('Unsupported command ' + id);
      return await callback(...args);
    }
  }
};
const languageSession = process.env.DOTDIR_VSCODE_LANGUAGE === '1' ? new LanguageSession(api) : undefined;
const apiKey = Symbol.for('dotdir.vscode.api');
globalThis[apiKey] = api;
const apiUrl = pathToFileURL(resolve('.dotdir-vscode-api.cjs')).href;
const apiSource = Object.keys(api)
  .map((key) => 'exports.' + key + ' = globalThis[Symbol.for("dotdir.vscode.api")].' + key + ';')
  .join('\n');
registerHooks({
  resolve(name, context, next) {
    return name === 'vscode'
      ? { url: apiUrl, format: 'commonjs', shortCircuit: true }
      : next(name, context);
  },
  load(url, context, next) {
    return url === apiUrl
      ? { format: 'commonjs', source: apiSource, shortCircuit: true }
      : next(url, context);
  }
});

async function disposeSubscriptions(subscriptions) {
  for (const disposable of subscriptions.reverse()) {
    try { await disposable.dispose(); } catch (error) { console.error(error); }
  }
}

async function activate(job, request, context = {}) {
  const root = realpathSync(request.root);
  const require = createRequire(resolve(root, 'package.json'));
  const entry = realpathSync(require.resolve(resolve(root, request.main)));
  const inside = relative(root, entry);
  if (inside === '..' || inside.startsWith('..' + sep) || isAbsolute(inside)) {
    throw new Error('Extension entry point is outside its package');
  }
  const module = await import(pathToFileURL(entry).href);
  const extension = module.default?.activate ? module.default : module;
  const subscriptions = [];
  const metadata = context.extension ?? {
    id: request.extension,
    extensionPath: root,
    extensionUri: Uri.file(root),
    packageJSON: request.localization?.packageJson ?? JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8')),
    isActive: true
  };
  try {
    const exports = await extension.activate?.({
      ...context,
      extension: metadata,
      subscriptions,
      extensionPath: root,
      asAbsolutePath: (path) => resolve(root, path)
    });
    Object.defineProperty(metadata, 'exports', { value: exports });
  } catch (error) {
    await disposeSubscriptions(subscriptions);
    throw error;
  }
  job.extension.subscriptions = subscriptions;
  extensions.set(request.extension, job.extension);
  flushCompileCache();
  return async () => {
    try { await extension.deactivate?.(); } finally { await disposeSubscriptions(subscriptions); }
  };
}

async function run(request) {
  const job = {
    id: request.id,
    extensionId: request.extension,
    extension: extensions.get(request.extension) ?? { commands: new Map() },
    configuration: request.configuration,
    edits: new Map(),
    transactions: []
  };
  job.extension.localization = request.localization;
  job.editor = textEditor(job, request.editor);
  current = job;
  let error = null;
  try {
    if (!extensions.has(request.extension)) await activate(job, request);
    await api.commands.executeCommand(request.command);
  } catch (failure) {
    error = errorDetails(failure);
  } finally {
    await Promise.allSettled(job.transactions);
    current = undefined;
    send({ type: 'done', id: request.id, error });
  }
}

if (languageSession) {
  languageSession.registeredCommands = () => [...(current?.extension.commands.keys() ?? [])];
  languageSession.start(async (request, context) => {
    current = { extensionId: request.extension, extension: { commands: new Map(), localization: request.localization } };
    languageSession.deactivate = await activate(current, request, context);
  });
} else {
  const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
  input.on('line', (line) => {
    const message = JSON.parse(line);
    if (message.type === 'ack') {
      if (current?.id !== message.id) return;
      const finish = current.edits.get(message.serial);
      current.edits.delete(message.serial);
      finish?.(message.editor);
    } else if (message.type === 'run' && !current) {
      void commandLog.run(message.extension, () => run(message));
    }
  });
}
