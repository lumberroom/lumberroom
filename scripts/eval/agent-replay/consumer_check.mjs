// Serialize each structured answer as the OpenClaw plugin does, `JSON.stringify(structured ?? {})`
// (src/tools.ts:64), and print the parsed result for retrieval_diff.py to compare.
//
// stdin: a JSON array of structured answers. stdout: the array after the round trip.
import { readFileSync } from "node:fs";

const answers = JSON.parse(readFileSync(0, "utf8"));
const out = answers.map((structured) => JSON.parse(JSON.stringify(structured ?? {})));
process.stdout.write(JSON.stringify(out));
