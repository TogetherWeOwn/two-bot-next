// Run with Node 24 against a read-only legacy source export pinned below:
// node generate_classifier_legacy.mjs /path/to/legacy > classifier_legacy.json
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createHash } from 'node:crypto';

const root = resolve(process.argv[2]);
const load = (path) => import(pathToFileURL(resolve(root, path)).href);
const sha256 = (path) => createHash('sha256').update(readFileSync(resolve(root, path))).digest('hex');
const hashes = {
  'src/analytics/communityClassifier.ts': '61cd972a4667c6f426b74890d28875f588f7b3e5c010911ada9d6b5a4dd90982',
  'src/core/inviteTracker.ts': '2cbe843832e62924a7b4e28fabc04de29598edfef1b3fe9987f4b5850be16419',
  'src/analytics/communityFacts.ts': 'd2b8a24ef88611f9f4a83e88ee5bc7c4c4df6bbebfec7e7621093bf9de9a8b99',
  'src/core/handlers.ts': 'ec5a9d97947bfb0a3257aa758a5b1e09028dcb0d5deaeece8123dc1dfcb0127a',
  'src/core/events.ts': '4c7ea39081aeb24ab3f8af62a77488c1f4d54dc9bfaca6b6d6dbfca0b4a5ec2f',
  'src/core/voiceSessions.ts': '9f966b84c2dec505e7bc2a3e432d307c819556b0f0f100b997c489f57fdc9262',
  'src/core/log.ts': '0900342c2ecd61a099b0fa111cf52f82fde488c573bd8274a85649498bac70c5',
  'test/fixtures/funnel-attribution-golden.json': '1d0606849dbbe3c1a53a14f6b195e3344d8f736d82233487b9c9171c76fa94bf',
};
for (const [path, hash] of Object.entries(hashes)) assert.equal(sha256(path), hash, `legacy source drift: ${path}`);
const { CommunityClassifier, loadCommunityClassifierConfig } = await load('src/analytics/communityClassifier.ts');
const { InviteTracker, inviteGrowth, attributeJoins, attributionCategory, summarizeAttributionSplit } = await load('src/core/inviteTracker.ts');
const { CommunityFactStore } = await load('src/analytics/communityFacts.ts');
const legacy = JSON.parse(readFileSync(resolve(root, 'test/fixtures/funnel-attribution-golden.json'), 'utf8'));
const env = {
  TWO_COMMUNITY_CLASSIFIER_VERSION: 'community-test-v1',
  TWO_COMMUNITY_AUTOMATION_ACTOR_IDS: 'staff-bot',
  TWO_COMMUNITY_RAID_ACTOR_IDS: 'raid-user',
  TWO_COMMUNITY_STAGING_GUILD_IDS: 'staging-guild',
  TWO_COMMUNITY_STAGING_ACTOR_IDS: 'staging-user',
  TWO_COMMUNITY_TEST_ACTOR_IDS: 'test-user',
};
const classifier = new CommunityClassifier(loadCommunityClassifierConfig(env));
const community = [
  ['bot-precedes-webhook', { actorId: 'x', isBot: true, webhookId: 'w' }],
  ['webhook', { actorId: 'x', webhookId: 'w' }],
  ['automation', { actorId: 'staff-bot' }],
  ['raid', { actorId: 'raid-user' }],
  ['staging-guild', { guildId: 'staging-guild', actorId: 'x' }],
  ['test-actor', { actorId: 'test-user' }],
  ['attendance-human', { actorId: 'human-1' }],
  ['attendance-bot', { actorId: 'bot-1', isBot: true }],
  ['staging-actor', { actorId: 'staging-user' }],
  ['explicit-automation', { actorId: 'x', isStaffAutomation: true }],
  ['explicit-raid', { actorId: 'x', isRaid: true }],
  ['explicit-staging', { actorId: 'x', isStaging: true }],
  ['explicit-test', { actorId: 'x', isTest: true }],
  ['empty-webhook-human', { actorId: 'human-1', webhookId: '' }],
  ['all-exclusions-bot-wins', { actorId: 'test-user', isBot: true, webhookId: 'w', isStaffAutomation: true, isRaid: true, isStaging: true, isTest: true }],
  ['automation-precedes-raid', { actorId: 'raid-user', isStaffAutomation: true }],
  ['raid-precedes-staging', { actorId: 'staging-user', isRaid: true }],
  ['staging-precedes-test', { actorId: 'test-user', isStaging: true }],
].map(([id, input]) => {
  input = { guildId: 'guild-a', ...input };
  return { id, input, expect: classifier.classify(input) };
});
const tracker = new InviteTracker(null);
const invites = legacy.cases.map((c) => {
  if (c.kind === 'window') {
    const s = c.scenario;
    const growth = inviteGrowth(new Map(Object.entries(s.prevUses)), s.currentUses);
    const actual = attributeJoins(growth, s.joinCount, s.guildHasVanity);
    assert.deepEqual(actual.map((x) => x.source), c.expect.sources, c.id);
    assert.deepEqual(actual.map((x) => x.exact), c.expect.exact, c.id);
    return { ...c, growth: Object.fromEntries(growth) };
  }
  assert.equal(tracker.attribute(c.scenario.grew, c.scenario.guildHasVanity), c.expect.source, c.id);
  return c;
});
const sources = ['unknown', 'ambiguous', 'ambiguous:aaa+bbb', 'vanity', 'invite:aaa', 'invite:aB3xY9', 'invite:abc123', 'web:one_click', 'backfill:log:member-join', 'backfill:member_list', 'unknown:other'];
const categories = sources.map((source) => ({ source, expect: attributionCategory(source) }));
const splits = [
  [{ source: 'invite:aaa', n: 5 }, { source: 'ambiguous:aaa+bbb', n: 3 }, { source: 'ambiguous:ccc+ddd', n: 2 }, { source: 'unknown', n: 7 }, { source: 'vanity', n: 1 }],
  [],
  // Quality counts include the population below the display's top-15 cutoff.
  [...Array.from({ length: 15 }, (_, i) => ({ source: `invite:cutoff-${i}`, n: '2' })), { source: 'unknown', n: '1' }, { source: 'ambiguous:a+b', n: '1' }],
  [...Array.from({ length: 15 }, (_, i) => ({ source: `invite:cutoff-${i}`, n: '2' })), { source: 'unknown', n: '40' }, { source: 'ambiguous:a+b', n: '10' }, { source: 'ambiguous', n: '1' }, { source: 'ambiguous:c+d', n: '1' }, { source: 'vanity', n: '1' }, { source: 'unknown:other', n: '1' }],
].map((input) => ({ input, expect: summarizeAttributionSplit(input) }));
const attendanceInput = { guildId: 'guild-a', actorId: 'human-1', eventOccurrenceId: 'event-1', occurredAt: '2026-09-02T10:00:00.000Z', proof: 'host_checkin' };
const facts = new CommunityFactStore(null, classifier);
let attendance;
facts.record = async (fact) => { attendance = { input: attendanceInput, expect: { ...fact, metadata: JSON.stringify(fact.metadata) } }; return true; };
await facts.recordAttendance(attendanceInput);
const { setLogLevel } = await load('src/core/log.ts');
setLogLevel('error'); // Keep stdout exclusively the reproducible fixture.
const { FunnelHandlers } = await load('src/core/handlers.ts');
const voiceMetadata = [];
for (const startedAt of ['2026-09-20T12:10:00.000Z', null]) {
  let saved;
  const handlers = new FunnelHandlers({
    record: async (event) => { saved = event; return { inserted: true }; },
    touchActivity: async () => {},
  });
  if (startedAt) handlers.voiceSessions.start('guild-a', 'human-1', 'voice', startedAt);
  await handlers.onVoiceLeave({ guildId: 'guild-a', memberId: 'human-1', channelId: 'voice', isBot: false, occurredAt: '2026-09-20T12:20:00.000Z' });
  voiceMetadata.push({ input: saved.metadata, expect: JSON.stringify(saved.metadata) });
}
console.log(JSON.stringify({
  version: 1,
  source: {
    repository: 'TogetherWeOwn/two-bot',
    revision: '96777468472f23a02a1e97a43ffab3912fe5df2a',
    hashes,
  },
  env, community, invites, categories, splits, attendance, voiceMetadata,
  unsupported: {
    reason: 'Legacy isUnknownBucket includes backfill history; no equivalent public runtime helper in Next. Report arithmetic is intentionally dropped, but these classification expectations must not be replaced by attributionCategory (which correctly returns other for backfill).',
    source: 'test/unit.unknownattribution.test.ts:23-32',
    cases: [
      { source: 'unknown', expect: true },
      { source: 'backfill:log:member-join', expect: true },
      { source: 'backfill:member_list', expect: true },
      { source: 'ambiguous:aaa+bbb', expect: false },
      { source: 'vanity', expect: false },
      { source: 'invite:abc123', expect: false },
      { source: 'web:one_click', expect: false },
    ],
  },
}, null, 2));
