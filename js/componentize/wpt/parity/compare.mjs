// The parity comparator: the gate's judgment half. Reads the two legs'
// records (baseline.mjs, roundtrip.mjs) and holds the round trip to the
// baseline's pass set:
//
//   - A *loss* is a test the baseline passes that the round trip fails or
//     never registers. Every loss must appear in losses.js; a loss that
//     does not is a regression and fails the run.
//   - A recorded loss that is no longer observed also fails the run, so
//     progress lands as a reviewable diff and the file only shrinks
//     deliberately (`just wpt::update-losses`).
//   - A test the round trip passes and the baseline fails is reported but
//     does not gate: it is not a loss, but it is the shim diverging from
//     the platform, which the report should keep visible.
//   - WPT test names are outcome-dependent: a failed setup step registers
//     a synthetic step name ("generate wrong key step: ...") in place of
//     the real test's, so the two legs' name sets legitimately differ
//     where their outcomes do. Round-trip-only *failures* are therefore
//     expected (their baseline counterparts surface as losses). A
//     round-trip-only *pass* fails hard: a pass the baseline never
//     measured is outside the gate's premise, and one appearing is the
//     sign the legs' group tables have drifted (see baseline.mjs).
//   - The two legs must run the same group set; a group present in one
//     and not the other fails hard.
//
// Usage: node compare.mjs <baseline.json> <roundtrip.json> [--losses <file>] [--update]
//
// `--losses` names the ratchet file (default losses.js beside this module):
// each gated leg pair pins its own — the loss set is a fact about one
// engine's baseline, so the Node legs and each browser engine's legs
// ratchet separately.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { updateRecipe, writeLosses } from "./losses-file.mjs";

const args = process.argv.slice(2);
const positional = [];
let update = false;
let lossesFile = "losses.js";
for (let i = 0; i < args.length; i += 1) {
  if (args[i] === "--update") update = true;
  else if (args[i] === "--losses") {
    i += 1;
    lossesFile = args[i];
  } else positional.push(args[i]);
}
const [baselinePath, roundtripPath] = positional;
if (!baselinePath || !roundtripPath || positional.length !== 2 || lossesFile === undefined) {
  console.error("usage: node compare.mjs <baseline.json> <roundtrip.json> [--losses <file>] [--update]");
  process.exit(2);
}
const LOSSES_PATH = fileURLToPath(new URL(`./${lossesFile}`, import.meta.url));
const UPDATE_RECIPE = updateRecipe(LOSSES_PATH);

/**
 * Index records by `group :: name`, disambiguating registration-order
 * duplicates with a ` #n` suffix (both legs run the suites sequentially,
 * so order is stable).
 * @param {{ group: string, name: string, status: string, message?: string }[]} records
 */
function index(records) {
  const map = new Map();
  const seen = new Map();
  for (const record of records) {
    const base = `${record.group} :: ${record.name}`;
    const n = (seen.get(base) ?? 0) + 1;
    seen.set(base, n);
    map.set(n === 1 ? base : `${base} #${n}`, record);
  }
  return map;
}

const baseline = index(JSON.parse(readFileSync(baselinePath, "utf8")));
const roundtrip = index(JSON.parse(readFileSync(roundtripPath, "utf8")));

const problems = [];

const baselineGroups = new Set([...baseline.values()].map((record) => record.group));
const roundtripGroups = new Set([...roundtrip.values()].map((record) => record.group));
for (const group of new Set([...baselineGroups, ...roundtripGroups])) {
  if (!baselineGroups.has(group) || !roundtripGroups.has(group)) {
    problems.push(
      `group ${JSON.stringify(group)} ran in only one leg — the legs' group tables have drifted`,
    );
  }
}

const roundtripOnly = [...roundtrip.entries()].filter(([key]) => !baseline.has(key));
const unmeasuredPasses = roundtripOnly.filter(([, record]) => record.status === "PASS");
if (unmeasuredPasses.length > 0) {
  problems.push(
    `${unmeasuredPasses.length} test(s) pass in the round trip with no baseline measurement — ` +
      `the legs' group tables have drifted:\n` +
      unmeasuredPasses.slice(0, 10).map(([key]) => `    ${key}`).join("\n"),
  );
}

let baselinePassed = 0;
let roundtripPassed = 0;
const losses = [];
const exceeded = [];
for (const [key, record] of baseline) {
  const other = roundtrip.get(key);
  if (other?.status === "PASS") {
    roundtripPassed += 1;
  }
  if (record.status !== "PASS") {
    if (other?.status === "PASS") {
      exceeded.push(key);
    }
    continue;
  }
  baselinePassed += 1;
  if (other === undefined) {
    losses.push({ key, detail: "never registered in the round trip" });
  } else if (other.status !== "PASS") {
    losses.push({ key, detail: other.message ?? other.status });
  }
}

if (update) {
  writeLosses(LOSSES_PATH, losses.map((loss) => loss.key));
  console.log(
    `wpt parity: recorded ${losses.length} known losses ` +
      `(baseline ${baselinePassed}/${baseline.size} passed, round trip ${roundtripPassed})`,
  );
  process.exit(0);
}

const { KNOWN_LOSSES } = await import(LOSSES_PATH);
const known = new Set(KNOWN_LOSSES);
const newLosses = losses.filter((loss) => !known.has(loss.key));
const observed = new Set(losses.map((loss) => loss.key));
const stale = KNOWN_LOSSES.filter((key) => !observed.has(key));

if (newLosses.length > 0) {
  problems.push(
    `${newLosses.length} new parity loss(es) — the round trip fails tests the platform passes, ` +
      `beyond the recorded set:\n` +
      newLosses.map(({ key, detail }) => `    ${key}\n      ${detail}`).join("\n"),
  );
}
if (stale.length > 0) {
  problems.push(
    `${stale.length} recorded loss(es) were not observed (fixed, renamed, or no longer ` +
      `platform-passed) — re-record with \`just ${UPDATE_RECIPE}\` once the change is understood:\n` +
      stale.slice(0, 20).map((key) => `    ${key}`).join("\n"),
  );
}

console.log(
  `wpt parity: baseline ${baselinePassed}/${baseline.size} passed; ` +
    `round trip ${roundtripPassed} passed, ${losses.length} known losses, ` +
    `${roundtripOnly.length} setup-failure renames`,
);
if (exceeded.length > 0) {
  console.log(
    `note: the round trip passes ${exceeded.length} test(s) the platform fails ` +
      `(not a loss, but the shim diverges from the platform):`,
  );
  for (const key of exceeded.slice(0, 10)) {
    console.log(`    ${key}`);
  }
}
if (problems.length > 0) {
  console.error(`\nwpt parity gate failed:\n${problems.join("\n")}`);
  process.exit(1);
}

