import { pathToFileURL } from 'node:url';

const [entry, operation, json, size] = process.argv.slice(1);
const limit = Number(size);
const write = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);

async function finish(status, data) {
  await new Promise((resolve, reject) => {
    write(Buffer.concat([Buffer.from([status]), data]), error => error ? reject(error) : resolve());
  });
  process.exit(0);
}

try {
  const module = await import(pathToFileURL(entry).href);
  const handler = module[operation] ?? module.default?.[operation];
  if (typeof handler !== 'function') throw new Error(`Provider must export ${operation}`);
  const result = await handler(JSON.parse(json), limit);
  let data;
  if (operation === 'readDirectory') {
    if (!Array.isArray(result)) throw new Error('readDirectory must return an array');
    data = Buffer.from(JSON.stringify(result));
  } else if (typeof result === 'string' || result instanceof Uint8Array) {
    data = Buffer.from(result);
  } else {
    throw new Error('readFile must return a string or Uint8Array');
  }
  if (data.byteLength > limit) throw new Error('Provider result exceeds the size limit');
  await finish(0, data);
} catch (error) {
  await finish(1, Buffer.from(String(error?.message ?? error)).subarray(0, 8192));
}
