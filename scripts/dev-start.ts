/**
 * Development start, on Windows, Linux and macOS: stops the Kelpy running on the development home, builds the Web UI
 * and the executable, then starts Kelpy on that home in this terminal. Options are passed on to Kelpy:
 *
 *   bun scripts/dev-start.ts [kelpy options]     for example: bun scripts/dev-start.ts --shutdown-idle 30m
 *
 * Stopping comes first because Windows cannot replace an executable that is running. The home is
 * `target/dev-home`, under `CARGO_TARGET_DIR` when it is set.
 */
import { existsSync } from 'node:fs';
import { join } from 'node:path';

const root = join(import.meta.dir, '..');
const web = join(root, 'web');
const target = process.env.CARGO_TARGET_DIR ?? join(root, 'target');
const home = join(target, 'dev-home');
const kelpy = join(target, 'debug', process.platform === 'win32' ? 'kelpy.exe' : 'kelpy');

/** Runs a command in this terminal and returns its exit code. */
function run(cmd: string[], cwd = root): number {
  return Bun.spawnSync({ cmd, cwd, stdio: ['inherit', 'inherit', 'inherit'] }).exitCode ?? 1;
}

/** Runs a command that must succeed; otherwise ends the script with its exit code. */
function step(cmd: string[], cwd = root, hint = '') {
  const code = run(cmd, cwd);
  if (code !== 0) {
    console.error(`dev-start: \`${cmd.join(' ')}\` failed (exit code ${code}).${hint}`);
    process.exit(code);
  }
}

// Nothing to stop without the binary or the home. Exit code 1: Kelpy was not running.
if (existsSync(kelpy) && existsSync(home)) {
  run([kelpy, 'stop', '--home', home]);
}

if (!existsSync(join(web, 'node_modules'))) {
  step(['bun', 'install'], web);
}
step(['bun', 'run', 'build'], web);
step(
  ['cargo', 'build'],
  root,
  process.platform === 'win32'
    ? ' If kelpy.exe is in use, a Kelpy with another home runs from this folder:' +
        ' stop it with `kelpy stop --home <its home>`.'
    : '',
);

// Ctrl+C reaches Kelpy too, which stops in order: this script waits for it instead of returning the prompt early.
process.on('SIGINT', () => {});
const child = Bun.spawn({
  cmd: [kelpy, '--home', home, ...Bun.argv.slice(2)],
  cwd: root,
  stdio: ['inherit', 'inherit', 'inherit'],
});
process.exit(await child.exited);
