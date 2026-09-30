// Run against a scratch clone, never the legacy runtime or a live guild.
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [clone, output] = process.argv.slice(2);
if (!clone || !output) throw new Error('Usage: node scripts/export-legacy-registry.mjs <legacy-clone> <output>');
const root = resolve(clone);
const revision = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (revision !== 'd5d1179348feb9157bcac8c875de9399d4f5c76a') {
  throw new Error(`Expected docs/parity.md frozen legacy revision, got ${revision}`);
}
const load = (path) => import(pathToFileURL(resolve(root, path)).href);
const { ApplicationCommandManager } = createRequire(resolve(root, 'package.json'))('discord.js');
const { mergedCommandData } = await load('src/discord/commandRegistry.ts');
const { COMMUNITY_COMMAND_DATA, AUTOMATION_COMMAND_DATA, ANNOUNCEMENT_COMMAND_DATA } =
  await load('src/discord/commandNames.ts');
const { ROTA_ACKNOWLEDGEMENT_COMMAND } = await load('src/discord/rotaAcknowledgement.ts');
const { MODERATION_COMMAND_DATA } = await load('src/moderation/commands.ts');

// src/index.ts:660–666, with every feature enabled and no DB-backed custom rows.
// transformCommand is the same mapping guild.commands.set uses before REST.
// Retain BOTH attendance definitions: do not hide the legacy name collision.
const commands = mergedCommandData([], [
  ...COMMUNITY_COMMAND_DATA,
  ROTA_ACKNOWLEDGEMENT_COMMAND,
  ...AUTOMATION_COMMAND_DATA,
  ...ANNOUNCEMENT_COMMAND_DATA,
  ...MODERATION_COMMAND_DATA,
]).map((command) => ApplicationCommandManager.transformCommand(command));
writeFileSync(resolve(output), JSON.stringify({
  source: 'TogetherWeOwn/two-bot',
  revision,
  configuration: 'All builtins enabled (including staging-only rota); no DB custom rows',
  commands,
}, null, 2) + '\n');
