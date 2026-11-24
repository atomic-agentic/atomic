// SPDX-License-Identifier: Apache-2.0
/// <reference path="./raw-imports.d.ts" />
// Typed compiler intrinsics, not a module that an atom executes.
export interface TimeSlot { start: string; end: string }
export interface Meeting { title: string; start: string; minutes: number }
export declare function fitsWithin(meeting: Meeting, slot: TimeSlot): boolean;
export interface JudgeSpec { input: unknown; output: unknown }
type Runtime<R> = [R] extends [never] ? {} : { readonly context: R };
type Context<I, O, C, R> = { input: I; output: O; case: C } & Runtime<R>;
type Scorer<I, O, C, J extends { [K in keyof J]: JudgeSpec }, R> = {
  [K in keyof J]: {
    judge: K;
    with: (context: Context<I, O, C, R>) => J[K]['input'];
    score: (context: { judge: J[K]['output'] } & Runtime<R>) => number;
    min_mean: number;
    /** How far the mean may fall below a baseline revision's, between 0 and 1. */
    max_drop?: number;
  }
}[keyof J];
export interface AtomConfig<I, O, C = unknown, J extends { [K in keyof J]: JudgeSpec } = {}, R = never> {
  atomic?: 1;
  name?: string;
  version?: number;
  /** `a2a` is staged: the catalog checks it, and the production loader refuses it until remote atoms ship. */
  kind?: 'lite' | 'codex' | 'exec' | 'a2a';
  remote?: unknown;
  auth?: unknown;
  credentials?: unknown;
  runtime?: unknown;
  model?: string;
  sandbox?: 'read-only' | 'workspace-write';
  network?: false | {allow: string[]};
  tools?: unknown;
  approvals?: unknown;
  budget?: unknown;
  retry?: unknown;
  exec?: unknown;
  effects?: unknown;
  env?: unknown;
  context?: unknown;
  case?: unknown;
  judges?: unknown;
  evals?: string | {dir?: string; pass_rate?: number};
  examples?: {name: string; input: I; output: O}[];
  description?: string;
  trust?: 'external' | 'team' | 'agent';
  input?: unknown;
  output?: unknown;
  validate?: ((context: Context<I, O, C, R>) => boolean)[];
  scorers?: Record<string, Scorer<I, O, C, J, R>>;
}
