// SPDX-License-Identifier: Apache-2.0
// Host implementations of registered, typed, deterministic compiler intrinsics.
export function fitsWithin(meeting, slot) {
  function timestamp(value) {
    // Require an explicit offset. Never depend on the host's local timezone.
    if (typeof value !== 'string' || !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,3})?(?:Z|[+-]\d{2}:\d{2})$/.test(value)) throw new Error('expected RFC 3339 timestamp with explicit offset');
    const [year, month, day, hour, minute, second] = value.slice(0, 19).split(/[-T:]/).map(Number);
    const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
    const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if (month < 1 || month > 12 || day < 1 || day > days[month - 1] || hour > 23 || minute > 59 || second > 59) throw new Error('invalid timestamp');
    const time = Date.parse(value);
    if (!Number.isFinite(time)) throw new Error('invalid timestamp');
    return time;
  }
  const start = timestamp(meeting?.start), low = timestamp(slot?.start), high = timestamp(slot?.end);
  if (!Number.isSafeInteger(meeting?.minutes) || meeting.minutes <= 0) return false;
  const end = start + meeting.minutes * 60_000;
  return Number.isSafeInteger(end) && low <= start && end <= high && low <= high;
}
export const intrinsics = new Map([['fitsWithin', { arity: 2, run: fitsWithin }]]);
