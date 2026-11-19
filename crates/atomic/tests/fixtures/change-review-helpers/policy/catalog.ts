// SPDX-License-Identifier: Apache-2.0
export const regions = {
  eu: { label: 'EU production', extraReviewer: 'eu-operations' },
  us: { label: 'US production', extraReviewer: 'us-operations' },
} as const satisfies Record<string, { label: string; extraReviewer: string }>;
