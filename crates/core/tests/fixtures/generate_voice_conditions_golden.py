#!/usr/bin/env python3
"""Generate crates/core/tests/fixtures/voice_conditions_golden.json.

V6b independent golden corpus (TOG-12468): condition-evaluator oracle rows
authored from docs/voice-rooms.md V6 plus the legacy two-bot tempVoice runtime
state each head reads. 26 contexts are copied verbatim from
tests/voice_templates/corpus.json; DERIVED contexts are synthesised here and
named in the Rust test's allowlist, which checks both.

Row basis:
  spec   - determinate from docs/voice-rooms.md V6 alone.
  choice - pins a choice TOG-12189 documents in docs/voice-conditions-core.md
           (line cited) where V6 alone does not decide the outcome; rows that
           settle a shared-corpus ambiguity (condition-grammar, numeric-##,
           numeric-+#) also name its tests/voice_templates/coverage.json ID.
shared_case marks rows transcribed from the shared corpus; the Rust
structural test cross-checks input/expected against it.
"""
import hashlib
import json
import os

SHARED = json.load(open('tests/voice_templates/corpus.json'))
CTX = SHARED['contexts']
SPEC_SHA = SHARED['spec']['sha256']
assert SHARED['spec']['path'] == 'docs/voice-rooms.md'
with open('docs/voice-rooms.md', 'rb') as spec_file:
    mine = hashlib.sha256(spec_file.read()).hexdigest()
assert mine == SPEC_SHA, 'spec moved; update rows consciously'

SPEC_V6 = 'docs/voice-rooms.md §V6'
CORE = ('TOG-12189 docs/voice-conditions-core.md '
        '(feat/voice-conditions-core)')
AMBIGUITY = '; tests/voice_templates/coverage.json ambiguity '
CORE_KEYWORDS = (CORE + ' L40-55 keyword table: the fact each head reads'
                 + AMBIGUITY + 'condition-grammar')
CORE_NUMERIC = (CORE + ' L23-26 untyped conditions are false; L34 ##, +# '
                'and name tokens are not numeric' + AMBIGUITY)
CORE_SPLIT = CORE + ' L10-11 split at first top-level ?? then first //'
CORE_LITERAL = CORE + ' L13 a node without ?? stays literal'
CORE_CASE = (CORE + ' L21 keywords, counters and calendar names match '
             'case-insensitively; IDs exact')
CORE_OPERANDS = CORE + ' L28-34 numeric operands; blank value is false'
CORE_CALENDAR = (CORE + ' L35 WEEKDAY/MONTH compare by position or full '
                 'English name')
CORE_SPACING = CORE + ' L83 branch text keeps spacing around ?? and //'
LEGACY_COUNT = ('two-bot src/tempVoice/nameFilter.ts renderNameTemplate '
                '{count}/{seq}; service.ts reserveIfUnderCaps '
                'countForOwner/countForGuild')
LEGACY_LIMIT = ('two-bot src/tempVoice/service.ts setLimit 0-99, '
                '0 = unlimited ("User limit removed")')
LEGACY_LOCK = ('two-bot src/tempVoice/service.ts lock/hide @everyone '
               'overwrite edits (Connect / ViewChannel)')
LEGACY_OWNER = ('two-bot src/tempVoice/service.ts claim/transfer, '
                'persisted row.ownerId')
LEGACY_ROLE = ('two-bot src/tempVoice/service.ts permit/reject role '
               'overwrites; role membership from Discord guild state')
LEGACY_NEW = ('no legacy conditional; legacy naming renders only '
              '{username}/{count}/{seq} (nameFilter.ts renderNameTemplate); '
              'state read from Discord gateway voice states '
              '(service.ts occupantsOf)')
LEGACY_NAME = ('legacy names carry no game/live/time tokens '
               '(nameFilter.ts); V5 tail trim→truncate→fallback '
               '(crates/core/src/voice_naming.rs render)')

CASES = [
    # ---- game heads (spec: GAME `:` contains; `=`/`!=` exact) ----
    ('v6b-game-contains', '{{GAME:Ape ??yes//no}}', 'game-alias', 'yes',
     ['condition:GAME'], 'spec', SPEC_V6, 'game-condition-GAME:Ape'),
    ('v6b-game-equals', '{{GAME=Apex ??yes//no}}', 'game-alias', 'yes',
     ['condition:GAME'], 'spec', SPEC_V6, 'game-condition-GAME=Apex'),
    ('v6b-game-not-equals-false', '{{GAME!=Apex ??yes//no}}', 'game-alias',
     'no', ['condition:GAME'], 'spec', SPEC_V6, 'game-condition-GAME!=Apex'),
    ('v6b-game-equals-minority-miss', '{{GAME=Chess ??yes//no}}',
     'game-alias', 'no', ['condition:GAME'], 'spec', SPEC_V6,
     'game-condition-GAME=Chess'),
    ('v6b-game-title-case-insensitive', '{{GAME=apex ??yes//no}}',
     'game-apex', 'yes', ['condition:GAME', 'rule:case'], 'choice', CORE_CASE,
     None),
    ('v6b-game-bare-shown', '{{GAME ??yes//no}}', 'game-apex', 'yes',
     ['condition:GAME'], 'spec', SPEC_V6, None),
    ('v6b-game-bare-empty', '{{GAME ??yes//no}}', 'solo', 'no',
     ['condition:GAME'], 'spec', SPEC_V6, None),
    # ---- perms: person heads (spec table; case rows are TOG-12189 choices) ----
    ('v6b-role-hit', '{{ROLE:raid ??yes//no}}', 'role-owner', 'yes',
     ['condition:ROLE:id'], 'spec', SPEC_V6, 'role-role-owner'),
    ('v6b-role-miss', '{{ROLE:raid ??yes//no}}', 'solo', 'no',
     ['condition:ROLE:id'], 'spec', SPEC_V6, 'role-solo'),
    ('v6b-role-scope-case-insensitive', '{{role:raid ??yes//no}}',
     'role-owner', 'yes', ['condition:ROLE:id', 'rule:case'], 'choice',
     CORE_CASE, None),
    ('v6b-role-id-exact', '{{ROLE:Raid ??yes//no}}', 'role-owner', 'no',
     ['condition:ROLE:id', 'rule:case'], 'choice', CORE_CASE, None),
    ('v6b-any-role-member', '{{ANY_ROLE:raid ??yes//no}}', 'role-other',
     'yes', ['condition:ANY_ROLE:id'], 'spec', SPEC_V6,
     'any-role-role-other'),
    ('v6b-any-role-absent', '{{ANY_ROLE:raid ??yes//no}}', 'solo', 'no',
     ['condition:ANY_ROLE:id'], 'spec', SPEC_V6, 'any-role-solo'),
    ('v6b-member-hit', '{{MEMBER:owner ??yes//no}}', 'solo', 'yes',
     ['condition:MEMBER:id'], 'spec', SPEC_V6,
     'person-condition-MEMBER:owner'),
    ('v6b-member-miss', '{{MEMBER:absent ??yes//no}}', 'solo', 'no',
     ['condition:MEMBER:id'], 'spec', SPEC_V6,
     'person-condition-MEMBER:absent'),
    ('v6b-owner-id-hit', '{{OWNER:owner ??yes//no}}', 'solo', 'yes',
     ['condition:OWNER:id'], 'spec', SPEC_V6,
     'person-condition-OWNER:owner'),
    ('v6b-owner-id-miss', '{{OWNER:absent ??yes//no}}', 'solo', 'no',
     ['condition:OWNER:id'], 'spec', SPEC_V6,
     'person-condition-OWNER:absent'),
    ('v6b-owner-bare-present', '{{OWNER ??yes//no}}', 'solo', 'yes',
     ['condition:OWNER'], 'choice', CORE_KEYWORDS, None),
    # ---- perms: room state (spec: FULL requires a limit; PRIVATE never standalone) ----
    ('v6b-full-at-limit', '{{FULL ??full//open}}', 'full-full', 'full',
     ['condition:FULL'], 'spec', SPEC_V6, 'full-full'),
    ('v6b-full-with-space', '{{FULL ??full//open}}', 'full-space', 'open',
     ['condition:FULL'], 'spec', SPEC_V6, 'full-space'),
    ('v6b-full-unlimited', '{{FULL ??full//open}}', 'full-unlimited',
     'open', ['condition:FULL'], 'spec', SPEC_V6, 'full-unlimited'),
    ('v6b-full-keyword-case-insensitive', '{{full ??full//open}}',
     'full-full', 'full', ['condition:FULL', 'rule:case'], 'choice', CORE_CASE,
     None),
    ('v6b-private-temp-locked', '{{PRIVATE ??private//public}}',
     'private-temporary-True', 'private', ['condition:PRIVATE'], 'spec',
     SPEC_V6, 'private-temporary-True'),
    ('v6b-private-temp-open', '{{PRIVATE ??private//public}}',
     'private-temporary-False', 'public', ['condition:PRIVATE'], 'spec',
     SPEC_V6, 'private-temporary-False'),
    ('v6b-private-standalone-never', '{{PRIVATE ??private//public}}',
     'private-standalone-True', 'public', ['condition:PRIVATE'], 'spec',
     SPEC_V6, 'private-standalone-True'),
    # ---- activity / streaming (choice: TOG-12189 keyword table) ----
    ('v6b-playing-idle', '{{PLAYING ??yes//no}}', 'live-offline', 'no',
     ['condition:PLAYING'], 'choice', CORE_KEYWORDS, None),
    ('v6b-playing-active', '{{PLAYING ??yes//no}}', 'game-apex', 'yes',
     ['condition:PLAYING'], 'choice', CORE_KEYWORDS, None),
    ('v6b-live-offline', '{{LIVE ??yes//no}}', 'live-offline', 'no',
     ['condition:LIVE'], 'choice', CORE_KEYWORDS, None),
    ('v6b-live-discord', '{{LIVE ??yes//no}}', 'live-discord', 'yes',
     ['condition:LIVE'], 'choice', CORE_KEYWORDS, None),
    ('v6b-live-external', '{{LIVE ??yes//no}}', 'live-external', 'yes',
     ['condition:LIVE'], 'choice', CORE_KEYWORDS, None),
    ('v6b-live-discord-miss-external',
     '{{LIVE_DISCORD ??yes//no}}', 'live-external', 'no',
     ['condition:LIVE_DISCORD'], 'choice', CORE_KEYWORDS,
     None),
    ('v6b-live-external-miss-discord',
     '{{LIVE_EXTERNAL ??yes//no}}', 'live-discord', 'no',
     ['condition:LIVE_EXTERNAL'], 'choice', CORE_KEYWORDS,
     None),
    ('v6b-any-live', '{{ANY_LIVE ??yes//no}}', 'live-both', 'yes',
     ['condition:ANY_LIVE'], 'choice', CORE_KEYWORDS, None),
    ('v6b-any-live-offline', '{{ANY_LIVE ??yes//no}}', 'live-offline',
     'no', ['condition:ANY_LIVE'], 'choice', CORE_KEYWORDS,
     None),
    ('v6b-live-keyword-case-insensitive', '{{live ??yes//no}}',
     'live-discord', 'yes', ['condition:LIVE', 'rule:case'], 'choice',
     CORE_KEYWORDS, None),
    # ---- party heads (choice: TOG-12189 keyword table) ----
    ('v6b-players-three', '{{PLAYERS ??yes//no}}', 'playing-no-party-3',
     'yes', ['condition:PLAYERS'], 'choice', CORE_KEYWORDS,
     None),
    ('v6b-players-none', '{{PLAYERS ??yes//no}}', 'solo', 'no',
     ['condition:PLAYERS'], 'choice', CORE_KEYWORDS, None),
    ('v6b-max-capped', '{{MAX ??yes//no}}', 'v6b-party-capped', 'yes',
     ['condition:MAX'], 'choice', CORE_KEYWORDS, None),
    ('v6b-max-uncapped', '{{MAX ??yes//no}}', 'party-4', 'no',
     ['condition:MAX'], 'choice', CORE_KEYWORDS, None),
    ('v6b-max-no-advertised-max', '{{MAX ??yes//no}}',
     'party-no-maximum', 'no', ['condition:MAX'], 'choice',
     CORE_KEYWORDS, None),
    ('v6b-rich-party', '{{RICH ??yes//no}}', 'party-4', 'yes',
     ['condition:RICH'], 'choice', CORE_KEYWORDS, None),
    ('v6b-rich-none', '{{RICH ??yes//no}}', 'solo', 'no',
     ['condition:RICH'], 'choice', CORE_KEYWORDS, None),
    # ---- time heads ----
    ('v6b-weekend-saturday', '{{WEEKEND ??weekend//weekday}}',
     'day-Saturday', 'weekend', ['condition:WEEKEND'], 'spec', SPEC_V6,
     'weekend-Saturday'),
    ('v6b-weekend-sunday', '{{WEEKEND ??weekend//weekday}}', 'day-Sunday',
     'weekend', ['condition:WEEKEND'], 'spec', SPEC_V6, 'weekend-Sunday'),
    ('v6b-weekend-monday', '{{WEEKEND ??weekend//weekday}}', 'day-Monday',
     'weekday', ['condition:WEEKEND'], 'spec', SPEC_V6, 'weekend-Monday'),
    ('v6b-weekday-bare-monday', '{{WEEKDAY ??weekday//weekend}}',
     'day-Monday', 'weekday', ['condition:WEEKDAY'], 'choice',
     CORE_KEYWORDS, None),
    ('v6b-weekday-name-equals', '{{WEEKDAY:Monday ??yes//no}}',
     'day-Monday', 'yes', ['condition:WEEKDAY'], 'choice',
     CORE_CALENDAR, None),
    ('v6b-weekday-name-miss', '{{WEEKDAY:Friday ??yes//no}}', 'day-Monday',
     'no', ['condition:WEEKDAY'], 'choice', CORE_CALENDAR, None),
    ('v6b-month-number', '{{MONTH = 9 ??yes//no}}', 'month-September',
     'yes', ['condition:MONTH', 'compare:='], 'choice', CORE_CALENDAR, None),
    ('v6b-month-name', '{{MONTH:September ??yes//no}}', 'month-September',
     'yes', ['condition:MONTH'], 'choice', CORE_CALENDAR, None),
    ('v6b-month-name-miss', '{{MONTH:April ??yes//no}}',
     'month-September', 'no', ['condition:MONTH'], 'choice',
     CORE_CALENDAR, None),
    ('v6b-month-case-insensitive', '{{month:september ??yes//no}}',
     'month-September', 'yes', ['condition:MONTH', 'rule:case'], 'choice',
     CORE_CALENDAR, None),
    ('v6b-hour-early', '{{@@hour@@ < 12 ??early//late}}', 'hour-0',
     'early', ['token:@@hour@@', 'compare:<'], 'spec', SPEC_V6, None),
    ('v6b-hour-late', '{{@@hour@@ < 12 ??early//late}}', 'hour-23',
     'late', ['token:@@hour@@', 'compare:<'], 'spec', SPEC_V6, None),
    # ---- count / comparison heads (spec: six ops, token-vs-token; other operands choice) ----
    ('v6b-cmp-lt-false', '{{@@num@@ < 1 ??yes//no}}', 'solo', 'no',
     ['compare:<', 'token:@@num@@'], 'spec', SPEC_V6, 'compare-equal-lt'),
    ('v6b-cmp-gt-false', '{{@@num@@ > 1 ??yes//no}}', 'solo', 'no',
     ['compare:>', 'token:@@num@@'], 'spec', SPEC_V6, 'compare-equal-gt'),
    ('v6b-cmp-le-true', '{{@@num@@ <= 1 ??yes//no}}', 'solo', 'yes',
     ['compare:<=', 'token:@@num@@'], 'spec', SPEC_V6, 'compare-equal-le'),
    ('v6b-cmp-ge-true', '{{@@num@@ >= 1 ??yes//no}}', 'solo', 'yes',
     ['compare:>=', 'token:@@num@@'], 'spec', SPEC_V6, 'compare-equal-ge'),
    ('v6b-cmp-eq-true', '{{@@num@@ = 1 ??yes//no}}', 'solo', 'yes',
     ['compare:=', 'token:@@num@@'], 'spec', SPEC_V6, 'compare-equal-eq'),
    ('v6b-cmp-ne-false', '{{@@num@@ != 1 ??yes//no}}', 'solo', 'no',
     ['compare:!=', 'token:@@num@@'], 'spec', SPEC_V6, 'compare-equal-ne'),
    ('v6b-cmp-token-vs-token', '{{@@num@@ < @@limit@@ ??yes//no}}',
     'humans-3-4', 'yes', ['compare:<', 'token:@@num@@',
                           'token:@@limit@@'], 'spec', SPEC_V6,
     'compare-num-limit'),
    ('v6b-cmp-slots-vs-limit', '{{@@slots@@ != @@limit@@ ??yes//no}}',
     'humans-1-2', 'yes', ['compare:!=', 'token:@@slots@@',
                           'token:@@limit@@'], 'spec', SPEC_V6,
     'compare-slots-limit'),
    ('v6b-cmp-hour-ge', '{{@@hour@@ >= 12 ??yes//no}}', 'solo', 'yes',
     ['compare:>=', 'token:@@hour@@'], 'spec', SPEC_V6, 'compare-hour'),
    ('v6b-cmp-room-number', '{{$# = 3 ??yes//no}}', 'solo', 'yes',
     ['compare:='], 'spec', SPEC_V6, 'compare-room-number'),
    ('v6b-cmp-room-number-padded', '{{$00# = 3 ??yes//no}}', 'solo',
     'yes', ['compare:='], 'choice', CORE_OPERANDS, None),
    ('v6b-cmp-num-others', '{{@@num_others@@ = 2 ??yes//no}}',
     'humans-3-4', 'yes', ['compare:=', 'token:@@num_others@@'], 'choice',
     CORE_OPERANDS, None),
    ('v6b-cmp-num-live', '{{@@num_live@@ = 1 ??yes//no}}', 'live-both',
     'yes', ['compare:=', 'token:@@num_live@@'], 'choice', CORE_OPERANDS, None),
    ('v6b-cmp-num-playing', '{{@@num_playing@@ = 3 ??yes//no}}',
     'playing-no-party-3', 'yes', ['compare:=', 'token:@@num_playing@@'],
     'choice', CORE_OPERANDS, None),
    ('v6b-cmp-party-size-max', '{{@@party_size@@ = 12 ??yes//no}}',
     'party-4', 'yes', ['compare:=', 'token:@@party_size@@'], 'choice',
     CORE_OPERANDS, None),
    ('v6b-cmp-counter-case-insensitive', '{{@@NUM@@ = 1 ??yes//no}}',
     'solo', 'yes', ['compare:=', 'token:@@num@@', 'rule:case'], 'choice',
     CORE_CASE, None),
    ('v6b-cmp-blank-slots-always-false',
     '{{@@slots@@ = 0 ??yes//no}}', 'solo', 'no',
     ['compare:=', 'token:@@slots@@'], 'choice', CORE_OPERANDS, None),
    ('v6b-cmp-literals', '{{7 > 2 ??yes//no}}', 'solo', 'yes',
     ['compare:>'], 'spec', SPEC_V6, 'compare-literal'),
    # ---- unknown-head refusal (spec: an unknown condition is false) ----
    ('v6b-unknown-word', '{{NOT_DEFINED ??yes//no}}', 'solo', 'no',
     ['rule:unknown-condition'], 'spec', SPEC_V6, 'condition-unknown'),
    ('v6b-unknown-name-token-not-expanded',
     '{{@@owner@@ = Alex ??yes//no}}', 'solo', 'no',
     ['rule:unknown-condition', 'rule:name-in-condition',
      'token:@@owner@@'], 'spec', SPEC_V6, 'condition-name-not-expanded'),
    ('v6b-unknown-hash-not-numeric', '{{## = 3 ??yes//no}}', 'solo',
     'no', ['rule:unknown-condition', 'token:##'], 'choice',
     CORE_NUMERIC + 'numeric-##', None),
    ('v6b-unknown-roman-not-numeric', '{{+# = 3 ??yes//no}}', 'solo',
     'no', ['rule:unknown-condition', 'token:+#'], 'choice',
     CORE_NUMERIC + 'numeric-+#', None),
    ('v6b-unknown-person-with-compare', '{{ROLE=raid ??yes//no}}',
     'role-owner', 'no', ['rule:unknown-condition', 'condition:ROLE:id'],
     'spec', SPEC_V6, None),
    ('v6b-unknown-empty-condition', '{{ ??yes//no}}', 'solo', 'no',
     ['rule:unknown-condition'], 'spec', SPEC_V6, None),
    ('v6b-unknown-game-operator', '{{GAME > Apex ??yes//no}}',
     'game-apex', 'no', ['rule:unknown-condition', 'condition:GAME'],
     'spec', SPEC_V6, None),
    # ---- nesting (spec: blocks nest; `// no` optional) ----
    ('v6b-nested-else-hit', '{{ROLE:raid ??role//{{PRIVATE ??private//default}}}}',
     'role-owner', 'role', ['rule:nested', 'condition:ROLE:id',
                            'condition:PRIVATE'], 'spec', SPEC_V6,
     'nested-role-owner'),
    ('v6b-nested-else-inner', '{{ROLE:raid ??role//{{PRIVATE ??private//default}}}}',
     'private-temporary-True', 'private', ['rule:nested',
                                           'condition:ROLE:id',
                                           'condition:PRIVATE'], 'spec',
     SPEC_V6, 'nested-private-temporary-True'),
    ('v6b-nested-else-default',
     '{{ROLE:raid ??role//{{PRIVATE ??private//default}}}}', 'solo',
     'default', ['rule:nested', 'condition:ROLE:id',
                 'condition:PRIVATE'], 'spec', SPEC_V6, 'nested-solo'),
    ('v6b-nested-in-condition-true',
     '{{{{FULL ??FULL//PRIVATE}} ??yes//no}}', 'full-full', 'yes',
     ['rule:nested', 'condition:FULL', 'condition:PRIVATE'], 'spec',
     SPEC_V6, None),
    ('v6b-nested-in-condition-false',
     '{{{{FULL ??FULL//PRIVATE}} ??yes//no}}', 'full-unlimited', 'no',
     ['rule:nested', 'condition:FULL', 'condition:PRIVATE'], 'spec',
     SPEC_V6, None),
    ('v6b-optional-else-false-renders-nothing',
     'prefix{{PRIVATE ??secret}}', 'solo', 'prefix',
     ['rule:optional-else', 'condition:PRIVATE'], 'spec', SPEC_V6,
     'optional-else-false'),
    ('v6b-optional-else-true', 'prefix{{ROLE:raid ??role}}', 'role-owner',
     'prefixrole', ['rule:optional-else', 'condition:ROLE:id'], 'spec',
     SPEC_V6, 'optional-else-true'),
    # ---- verbatim output (choice: TOG-12189 keeps branch text as written) ----
    ('v6b-verbatim-extra-separators-true', '{{FULL ??a??b//c//d}}',
     'full-full', 'a??b', ['rule:verbatim', 'condition:FULL'], 'choice',
     CORE_SPLIT, None),
    ('v6b-verbatim-extra-separators-false', '{{FULL ??a??b//c//d}}',
     'full-space', 'c//d', ['rule:verbatim', 'condition:FULL'], 'choice',
     CORE_SPLIT, None),
    ('v6b-verbatim-spaces-kept', '[{{ FULL ?? yes // no }}]',
     'full-full', '[ yes ]', ['rule:verbatim', 'condition:FULL'], 'choice',
     CORE_SPACING, None),
    ('v6b-verbatim-no-node-stays-literal', '{{FULL}}', 'solo',
     '{{FULL}}', ['rule:verbatim', 'condition:FULL'], 'choice', CORE_LITERAL,
     None),
]

LEGACY_FOR = {
    'condition:GAME': LEGACY_NEW,
    'condition:ROLE:id': LEGACY_ROLE,
    'condition:ANY_ROLE:id': LEGACY_ROLE,
    'condition:MEMBER:id': LEGACY_OWNER,
    'condition:OWNER:id': LEGACY_OWNER,
    'condition:OWNER': LEGACY_OWNER,
    'condition:FULL': LEGACY_LIMIT,
    'condition:PRIVATE': LEGACY_LOCK,
    'condition:PLAYING': LEGACY_NEW,
    'condition:LIVE': LEGACY_NEW,
    'condition:LIVE_DISCORD': LEGACY_NEW,
    'condition:LIVE_EXTERNAL': LEGACY_NEW,
    'condition:ANY_LIVE': LEGACY_NEW,
    'condition:PLAYERS': LEGACY_NEW,
    'condition:MAX': LEGACY_NEW,
    'condition:RICH': LEGACY_NEW,
    'condition:WEEKEND': LEGACY_NEW,
    'condition:WEEKDAY': LEGACY_NEW,
    'condition:MONTH': LEGACY_NEW,
    'compare:': LEGACY_COUNT,
    'token:': LEGACY_COUNT,
    'rule:unknown-condition': LEGACY_NAME,
    'rule:name-in-condition': LEGACY_NAME,
    'rule:nested': LEGACY_NAME,
    'rule:optional-else': LEGACY_NAME,
    'rule:verbatim': LEGACY_NAME,
}


def legacy_for(covers):
    for tag in covers:
        key = tag if tag in LEGACY_FOR else tag.split(':')[0] + ':'
        if key in LEGACY_FOR:
            return LEGACY_FOR[key]
    return LEGACY_NEW


def derived_contexts():
    """Contexts the shared corpus lacks. Keep in sync with DERIVED_CONTEXTS
    in voice_conditions_golden.rs."""
    capped = json.loads(json.dumps(CTX['party-4']))
    capped['members'][0]['party'] = {
        'id': 'p1', 'size': 4, 'maximum': 4,
        'state': 'Ranked', 'details': 'Full squad'}
    return {'v6b-party-capped': capped}


def main():
    derived = derived_contexts()
    assert not set(derived) & set(CTX), 'derived context shadows shared'
    contexts = {}
    for key in sorted({c for _, _, c, _, _, _, _, _ in CASES}):
        contexts[key] = derived[key] if key in derived else CTX[key]
    cases = []
    for cid, src, ctx, exp, covers, basis, spec_cite, shared in CASES:
        if shared is not None:
            orig = next(c for c in SHARED['cases'] if c['id'] == shared)
            assert orig['input'] == src, cid
            assert orig['context'] == ctx, cid
            assert orig['expected']['kind'] == 'exact', cid
            assert orig['expected']['output'] == exp, cid
        row = {'id': cid, 'input': src, 'context': ctx,
               'expected': {'kind': 'exact', 'output': exp},
               'covers': covers, 'basis': basis,
               'source': {'spec': SPEC_V6 + '; ' + spec_cite
                          if spec_cite != SPEC_V6 else SPEC_V6,
                          'legacy': legacy_for(covers)}}
        if shared is not None:
            row['shared_case'] = shared
        cases.append(row)
    fixture = {'version': 1,
               'spec': {'path': 'docs/voice-rooms.md', 'sha256': SPEC_SHA},
               'description': ('V6b independent golden corpus over the '
                               'voice_conditions evaluator (TOG-12468). Rows '
                               'authored from docs/voice-rooms.md V6 plus '
                               'the legacy two-bot tempVoice runtime state '
                               'each head reads; basis=spec is determinate '
                               'from the spec alone, basis=choice pins a '
                               'choice TOG-12189 documents (line cited) where '
                               'V6 alone does not decide the outcome.'),
               'contexts': contexts, 'cases': cases}
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                 'voice_conditions_golden.json')
    with open(out, 'w') as fh:
        json.dump(fixture, fh, indent=1)
        fh.write('\n')
    print(f'{out}: {len(cases)} cases, {len(contexts)} contexts')


if __name__ == '__main__':
    main()
