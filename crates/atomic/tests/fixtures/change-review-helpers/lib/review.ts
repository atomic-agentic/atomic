// SPDX-License-Identifier: Apache-2.0
// Business rules for change review, as ordinary TypeScript. Each call runs in
// a fresh isolate with only its arguments.
import { regions } from '../policy/catalog.ts';
import type { Change } from '../.atomic/types/schemas/change';
import type { Evidence } from '../.atomic/types/schemas/evidence';
import type { Policy } from '../.atomic/types/schemas/policy';

export interface AssessInput { change: Change; policy: Policy; evidence: Evidence[] }

export function effectiveApprovers(change: Change, policy: Policy): string[] {
  const reviewers = [...policy.required_approvers];
  for (const [prefix, profile] of Object.entries(regions)) {
    if (change.service.regions.some((region) => region.startsWith(`${prefix}-`))) reviewers.push(profile.extraReviewer);
  }
  return [...new Set(reviewers)];
}

export function effectivePolicy(change: Change, policy: Policy): Policy {
  return { ...policy, required_approvers: effectiveApprovers(change, policy) };
}

export function missingEvidence({ policy, evidence }: AssessInput): string[] {
  const passed = new Set(evidence.filter((item) => item.status === 'passed').map((item) => item.kind));
  return policy.required_evidence.filter((kind) => !passed.has(kind));
}

export function evidenceInPolicyOrder({ policy, evidence }: AssessInput): Evidence[] {
  const required = new Set<string>(policy.required_evidence);
  const ordered = policy.required_evidence.flatMap((kind) => evidence.filter((item) => item.kind === kind));
  return [...ordered, ...evidence.filter((item) => !required.has(item.kind))];
}

export function reviewLabel(change: Change): string {
  const { service: { name, tier }, id } = change;
  return `${id}: ${name} (${tier})`;
}

// Computed from authored literals only, so it may supply instructions.
export function stageBanner(stage: string): string {
  return `Stage: ${stage.toUpperCase()}. Recommendations are advisory; only a human gate approves.`;
}
