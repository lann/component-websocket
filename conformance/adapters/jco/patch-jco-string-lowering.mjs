// Post-transpile fixup for a jco 1.26.1 string-lowering defect.
//
// `_lowerFlatStringUTF8` in the generated instantiation-mode output stores
// the string's *code point count* as the canonical-ABI length field. For
// UTF-8 the length field is the *byte* length, so any host-provided string
// whose byte length differs from its code point count (any non-ASCII
// string) is truncated mid-sequence: the guest lift then reads invalid
// UTF-8 and traps — or worse, silently drops trailing characters when the
// truncation happens to fall on a boundary.
//
// This script rewrites the two lines to store the byte length the encode
// helper already returns. It fails loudly when the expected code is not
// found, so a jco upgrade that fixes the defect surfaces as "remove this
// patch" rather than silently patching the wrong thing.
//
// Guarded by the `echo-text-unicode` conformance case: reverting this patch
// (or an unfixed jco upgrade) fails that row on both jco targets.
import { readFile, writeFile } from "node:fs/promises";

const path = process.argv[2] ?? "generated/conformance-guest.js";
const source = await readFile(path, "utf8");

const broken = `  const { ptr, codepoints } = _utf8AllocateAndEncode(ctx.vals[0], ctx.realloc, ctx.memory);
  
  const view = new DataView(ctx.memory.buffer);
  view.setUint32(ctx.storagePtr, ptr, true);
  view.setUint32(ctx.storagePtr + 4, codepoints, true);`;

const fixed = `  const { ptr, len } = _utf8AllocateAndEncode(ctx.vals[0], ctx.realloc, ctx.memory);
  
  const view = new DataView(ctx.memory.buffer);
  view.setUint32(ctx.storagePtr, ptr, true);
  view.setUint32(ctx.storagePtr + 4, len, true);`;

if (source.includes(fixed)) {
  console.error(`${path}: already patched`);
} else if (source.includes(broken)) {
  await writeFile(path, source.replace(broken, fixed));
  console.error(`${path}: patched _lowerFlatStringUTF8 to store byte length`);
} else {
  console.error(
    `${path}: expected _lowerFlatStringUTF8 shape not found - ` +
      `jco may have fixed the defect (remove this patch) or changed shape (update it)`,
  );
  process.exit(1);
}
