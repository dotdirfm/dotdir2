import { realpathSync } from 'node:fs';
import { createRequire, flushCompileCache, registerHooks } from 'node:module';
import { isAbsolute, relative, resolve, sep } from 'node:path';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';

// Stdout carries protocol messages only, including when an extension logs.
const protocol = process.stdout.write.bind(process.stdout);
const send = (message) => protocol(JSON.stringify(message) + '\n');
process.stdout.write = process.stderr.write.bind(process.stderr);

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
  Position,
  Range,
  Selection,
  EndOfLine: Object.freeze({ LF: 1, CRLF: 2 }),
  window: { createOutputChannel, get activeTextEditor() { return current?.editor; } },
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
      return { dispose() { if (commands.get(id) === command) commands.delete(id); } };
    },
    async executeCommand(id, ...args) {
      if (id === 'setContext') return;
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
  try {
    const exports = await extension.activate?.({
      ...context,
      subscriptions,
      extensionPath: root,
      asAbsolutePath: (path) => resolve(root, path)
    });
    if (context.extension) {
      Object.defineProperty(context.extension, 'exports', { value: exports });
    }
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
    extension: extensions.get(request.extension) ?? { commands: new Map() },
    configuration: request.configuration,
    edits: new Map(),
    transactions: []
  };
  job.editor = textEditor(job, request.editor);
  current = job;
  let error = null;
  try {
    if (!extensions.has(request.extension)) await activate(job, request);
    await api.commands.executeCommand(request.command);
  } catch (failure) {
    error = String(failure?.message ?? failure);
  } finally {
    await Promise.allSettled(job.transactions);
    current = undefined;
    send({ type: 'done', id: request.id, error });
  }
}

if (languageSession) {
  languageSession.start(async (request, context) => {
    current = { extension: { commands: new Map() } };
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
      void run(message);
    }
  });
}
