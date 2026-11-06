import { engineDeps, runEngine } from "./cli.ts";

const run = await runEngine(process.argv.slice(2), engineDeps(process.env));
process.stdout.write(run.stdout);
process.stderr.write(run.stderr);
// Set rather than exiting at once, so output to a pipe is flushed first.
process.exitCode = run.code;
