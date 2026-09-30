// Node 24; input directory contains service.ts and discord.ts from d5d11793.
// Run under en-US locale; stdout is leveling_legacy.json. See docs/leveling-port.md.
import fs from 'node:fs';
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const root = path.resolve(process.argv[2]);
const { totalXpForLevel, levelForXp } = await import(pathToFileURL(`${root}/service.ts`));
const source = fs.readFileSync(`${root}/discord.ts`, 'utf8');
const start = source.indexOf('export function rankText(');
const end = source.indexOf('export async function applyLevelRoles', start);
if (start < 0 || end < 0) throw new Error('rankText source boundary changed');
const rankFn = source.slice(start, end).trim()
  .replace('export function', 'function')
  .replace('profile: LevelProfile, displayName: string): string', 'profile, displayName)');
const rankText = new Function('totalXpForLevel', `${rankFn}; return rankText;`)(totalXpForLevel);
const levels = [...Array.from({ length: 101 }, (_, i) => i), 1000, 10000, 100000];
const curves = levels.map(level => ({ level, xp: totalXpForLevel(level) }));
const xpSamples = [...new Set([0, 59, 99, 114, 254, 9007199254740991,
  ...curves.flatMap(({ xp }) => xp ? [xp - 1, xp] : [0])])];
const profiles = [
  { display: 'Test', level: 1, rank: 3, memberCount: 50, xp: 114 },
  { display: 'Big Earner', level: 86, rank: 1200, memberCount: 10000, xp: 1234567 },
  { display: 'New Member', level: 0, rank: 1, memberCount: 0, xp: 0 },
].map(profile => ({ ...profile, content: rankText({ ...profile,
  nextLevelXp: totalXpForLevel(profile.level + 1) }, profile.display) }));
process.stdout.write(JSON.stringify({ legacy_revision: 'd5d11793',
  locale: Intl.DateTimeFormat().resolvedOptions().locale, curves,
  levels: xpSamples.map(xp => ({ xp, level: levelForXp(xp) })), profiles }, null, 2) + '\n');
