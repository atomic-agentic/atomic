// SPDX-License-Identifier: Apache-2.0
import sourceMap from 'source-map-js';

export function originalPosition(result) {
  const consumer = new sourceMap.SourceMapConsumer(result.map);
  return (at) => consumer.originalPositionFor({ line: at.line + 1, column: at.character });
}
