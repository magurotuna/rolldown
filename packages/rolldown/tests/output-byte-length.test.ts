import { expect, test, vi } from 'vitest';
import { rolldown } from '../src/api/rolldown';
import { outputByteLength } from '../src/cli/output-byte-length';

test('outputByteLength reads the length from Rust without converting the content to JS', async () => {
  const build = await rolldown({
    input: 'entry',
    plugins: [
      {
        name: 'test',
        resolveId: (id) => (id === 'entry' ? id : null),
        load: (id) => (id === 'entry' ? `export const text = 'héllo \u{1F30D}';` : null),
        buildStart() {
          this.emitFile({ type: 'asset', fileName: 'bytes.bin', source: new Uint8Array([0, 1, 255]) });
          this.emitFile({ type: 'asset', fileName: 'text.txt', source: 'ünïcode' });
        },
      },
    ],
  });
  const { output } = await build.generate({ sourcemap: true });
  await build.close();
  expect(output.map((item) => item.fileName).sort()).toEqual([
    'bytes.bin',
    'entry.js',
    'entry.js.map',
    'text.txt',
  ]);
  for (const item of output) {
    const content = vi.spyOn(item, item.type === 'chunk' ? 'code' : 'source', 'get');
    const length = outputByteLength(item);
    expect(content).not.toHaveBeenCalled();
    content.mockRestore();
    expect(length).toBe(Buffer.byteLength(item.type === 'chunk' ? item.code : item.source));
  }
});
