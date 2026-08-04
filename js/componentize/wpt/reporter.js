// The embedder half of the parity runner's `wpt:parity/reporter` import:
// the jco transpile maps that interface to this module (see the transpile
// scripts in parity/package.json), so whichever environment loads the
// generated module installs its sink here before invoking `run`, and
// receives each record as the test settles. Dependency-free and
// browser-safe.

let sink = null;

/** @param {((record: string) => void) | null} fn */
export function setSink(fn) {
  sink = fn;
}

export const reporter = {
  /** @param {string} record */
  report(record) {
    if (sink === null) {
      throw new Error("wpt parity reporter: no sink installed (call setSink before run)");
    }
    sink(record);
  },
};
