// A stand-in for the `atomic:std` module Atomic is planned to provide.
// Only the part this example uses.
export interface Context {
  /** Runs `run` once and saves its result; a retry or replay returns the saved result. */
  step<T>(name: string, run: () => Promise<T>): Promise<T>;
}
