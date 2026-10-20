import { OutputAssetImpl } from '../types/output-asset-impl';
import { OutputChunkImpl } from '../types/output-chunk-impl';
import type { OutputAsset, OutputChunk } from '../types/rolldown-output';

/**
 * Byte length of an output's `code` (chunk) or `source` (asset).
 *
 * For outputs produced by the bundler, Rust reports the length, so the content is not converted to a JS
 * string just to be measured. The CLI prints every output's size, and that conversion cost tens of ms for
 * builds with large chunks and sourcemaps.
 */
export function outputByteLength(output: OutputChunk | OutputAsset): number {
  if (output instanceof OutputChunkImpl) {
    return OutputChunkImpl.codeByteLength(output);
  }
  if (output instanceof OutputAssetImpl) {
    return OutputAssetImpl.sourceByteLength(output);
  }
  return Buffer.byteLength(output.type === 'chunk' ? output.code : output.source);
}
