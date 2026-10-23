#!/usr/bin/env node
"use strict";

// Daily digest release: tag the live production commit vYYYY.MM.DD and publish
// a GitHub Release whose notes come from the Conventional Commit subjects since
// the previous digest. One tag per day; quiet days (production already matches
// the last digest) skip green with no tag. Rollbacks serve an older commit and
// never get a tag, mirroring the retired per-promote guard.
//
// Note rendering is reused from the retired release-on-promote.cjs, with its
// fatal flaw fixed: git log ranges stream through a raised maxBuffer (32 MiB)
// instead of Node's default 1 MiB, which crashed with ENOBUFS on a 200-commit
// / 1.36 MB range. A regression test pins a 200-commit range above 1 MiB.
//
// Env: PROD_SHA (40-hex, must be on origin/main), GH_REPO, GITHUB_SERVER_URL,
// GITHUB_OUTPUT, DIGEST_DATE=YYYY-MM-DD (default today UTC), DRY_RUN=true to
// print without publishing. Needs a full-history checkout with tags.

const { execFileSync } = require("node:child_process");
const fs = require("node:fs");

const HEADER =
	/^(?<type>[a-z]+)(?:\((?<scope>[^()\r\n]+)\))?(?<bang>!)?: (?<desc>\S.*)$/;
const DIGEST_TAG = /^v(\d{4})\.(\d{2})\.(\d{2})$/;
const SEMVER_TAG = /^v(\d+)\.(\d+)\.(\d+)$/;
// 32 MiB covers ~20x the 1.36 MB range that crashed the 1 MiB default.
const LOG_MAX_BUFFER = 32 * 1024 * 1024;

function parseCommit(sha, subject, body) {
	const m = HEADER.exec(String(subject).trim());
	if (!m) return null;
	let desc = m.groups.desc.trim();
	let pr = null;
	const prMatch = / \(#(\d+)\)$/.exec(desc);
	if (prMatch) {
		pr = Number(prMatch[1]);
		desc = desc.slice(0, prMatch.index);
	}
	return {
		sha,
		type: m.groups.type,
		scope: m.groups.scope ? m.groups.scope.trim() : null,
		breaking:
			Boolean(m.groups.bang) || /^BREAKING[ -]CHANGE: /m.test(body || ""),
		desc,
		pr,
	};
}

function parseDigestTag(tag) {
	const m = DIGEST_TAG.exec(tag);
	if (!m) return null;
	const [, y, mo, d] = m;
	const dt = new Date(Date.UTC(Number(y), Number(mo) - 1, Number(d)));
	if (
		dt.getUTCFullYear() !== Number(y) ||
		dt.getUTCMonth() !== Number(mo) - 1 ||
		dt.getUTCDate() !== Number(d)
	)
		return null;
	return tag;
}

function parseTag(tag) {
	const m = SEMVER_TAG.exec(tag);
	return m ? m.slice(1, 4).map(Number) : null;
}

function compareVersions(a, b) {
	for (let i = 0; i < 3; i += 1) if (a[i] !== b[i]) return a[i] - b[i];
	return 0;
}

function loadOptions(path) {
	const pkg = JSON.parse(fs.readFileSync(path, "utf8")).packages["."];
	return {
		sections: pkg["changelog-sections"] || [],
	};
}

function entry(c, repoUrl) {
	const scope = c.scope ? `**${c.scope}:** ` : "";
	const pr = c.pr ? ` ([#${c.pr}](${repoUrl}/issues/${c.pr}))` : "";
	return `* ${scope}${c.desc}${pr} ([${c.sha.slice(0, 7)}](${repoUrl}/commit/${c.sha}))`;
}

function renderNotes({ tag, prevTag, date, commits, sections, repoUrl }) {
	const lines = [`## [${tag}](${repoUrl}/compare/${prevTag}...${tag}) (${date})`];
	const bySort = (a, b) =>
		(a.scope || "").localeCompare(b.scope || "") ||
		a.desc.localeCompare(b.desc);
	const breaking = commits.filter((c) => c.breaking).sort(bySort);
	if (breaking.length)
		lines.push(
			"",
			"",
			"### ⚠ BREAKING CHANGES",
			"",
			...breaking.map((c) => entry(c, repoUrl)),
		);
	const visible = new Map();
	for (const s of sections) {
		if (s.hidden) continue;
		if (!visible.has(s.section)) visible.set(s.section, new Set());
		visible.get(s.section).add(s.type);
	}
	let any = breaking.length > 0;
	for (const [section, types] of visible) {
		const items = commits.filter((c) => types.has(c.type)).sort(bySort);
		if (!items.length) continue;
		any = true;
		lines.push(
			"",
			"",
			`### ${section}`,
			"",
			...items.map((c) => entry(c, repoUrl)),
		);
	}
	if (!any)
		lines.push(
			"",
			"No user-facing changes in this release; the compare link lists every commit.",
		);
	return `${lines.join("\n")}\n`;
}

function git(...args) {
	return execFileSync("git", args, { encoding: "utf8" }).trim();
}

function gitLog(range) {
	const args =
		range === null
			? ["log", "--format=%H%x1f%s%x1f%b%x1e"]
			: ["log", "--format=%H%x1f%s%x1f%b%x1e", range];
	return execFileSync("git", args, {
		encoding: "utf8",
		maxBuffer: LOG_MAX_BUFFER,
	});
}

// Latest vYYYY.MM.DD digest tag by calendar date (zero-padded, so string sort
// orders chronologically), or null when there is none. Invalid calendar dates
// (v2026.13.99) are ignored, as are non-digest tags.
function latestDigestTag() {
	const tags = git("tag", "--list", "v*")
		.split("\n")
		.map((t) => t.trim())
		.filter((t) => parseDigestTag(t));
	tags.sort();
	return tags.length ? tags[tags.length - 1] : null;
}

// Latest digest tag strictly before a given tag (zero-padded dates sort
// chronologically as strings), or null when there is none. The repair path
// ranges its notes from this tag so a tag is never compared against itself.
function latestDigestTagBefore(tag) {
	const tags = git("tag", "--list", "v*")
		.split("\n")
		.map((t) => t.trim())
		.filter((t) => parseDigestTag(t) && t < tag);
	tags.sort();
	return tags.length ? tags[tags.length - 1] : null;
}

function tagTarget(tag) {
	return git("rev-list", "-n", "1", tag);
}

function isAncestor(sha, target) {
	try {
		git("merge-base", "--is-ancestor", sha, target);
		return true;
	} catch {
		return false;
	}
}

// Highest vX.Y.Z tag reachable from a SHA, for notes continuity on the first
// digest (which has no previous digest). Null when there is none.
function latestSemverReachable(sha) {
	return (
		git("tag", "--merged", sha, "--list", "v*")
			.split("\n")
			.map((t) => t.trim())
			.filter((t) => parseTag(t) && !parseDigestTag(t))
			.sort((a, b) => compareVersions(parseTag(b), parseTag(a)))[0] || null
	);
}

function releaseExists(tag) {
	try {
		execFileSync("gh", ["release", "view", tag, "--json", "tagName"], {
			stdio: "ignore",
		});
		return true;
	} catch {
		return false;
	}
}

// Release assets release.yml attaches to every digest (release.yml uploads
// sbom/*.cdx.json, SHA256SUMS, source-sha.txt and image-id.txt; sbom.yml
// names the two CycloneDX inventories). True when the release exists but
// lacks any of them. False when the assets are complete, when the release
// itself is missing (a DIGEST_DATE manual repair owns that), or when the
// lookup fails: a quiet day must never turn red or spam dispatches on a
// lookup error.
const SBOM_ASSETS = new Set([
	"rust-workspace.cdx.json",
	"container-image.cdx.json",
	"SHA256SUMS",
	"source-sha.txt",
	"image-id.txt",
]);

function sbomAssetsMissing(tag) {
	let raw;
	try {
		raw = execFileSync("gh", ["release", "view", tag, "--json", "assets"], {
			encoding: "utf8",
		});
	} catch {
		return false;
	}
	let assets;
	try {
		assets = JSON.parse(raw).assets || [];
	} catch {
		return false;
	}
	const names = new Set(assets.map((a) => a && a.name).filter(Boolean));
	for (const asset of SBOM_ASSETS) if (!names.has(asset)) return true;
	return false;
}

function output(values) {
	const text = Object.entries(values)
		.map(([k, v]) => `${k}=${v}\n`)
		.join("");
	if (process.env.GITHUB_OUTPUT)
		fs.appendFileSync(process.env.GITHUB_OUTPUT, text);
	process.stdout.write(text);
}

function main() {
	const sha = (process.env.PROD_SHA || "").trim().toLowerCase();
	const repo = process.env.GH_REPO;
	const dryRun = process.env.DRY_RUN === "true";
	const date = (process.env.DIGEST_DATE || "").trim() ||
		new Date().toISOString().slice(0, 10);
	if (!/^[0-9a-f]{40}$/.test(sha))
		throw new Error("PROD_SHA must be a full 40-character commit SHA");
	if (!repo) throw new Error("GH_REPO is required");
	if (!/^\d{4}-\d{2}-\d{2}$/.test(date))
		throw new Error("DIGEST_DATE must be YYYY-MM-DD");
	const digestTag = `v${date.replace(/-/g, ".")}`;
	if (!parseDigestTag(digestTag))
		throw new Error("DIGEST_DATE must be a real calendar date");
	const repoUrl = `${process.env.GITHUB_SERVER_URL || "https://github.com"}/${repo}`;
	try {
		git("merge-base", "--is-ancestor", sha, "origin/main");
	} catch {
		throw new Error(`${sha} is not on origin/main; refusing to tag it`);
	}

	const lastDigest = latestDigestTag();
	const latestFlag = !lastDigest || digestTag >= lastDigest ? "--latest" : "--latest=false";
	// Persist the owed state before any fallible note/publish operation. On a
	// failure these step outputs survive for the workflow's alert job; success
	// overwrites released below. Policy skips never call this helper.
	const pendingDigest = () => output({
		tag_name: "", released: "false", prod_changed: "true",
		prev_tag: lastDigest || "",
	});
	// Quiet and rollback days owe no tag, but may still owe SBOM assets: a
	// red run after yesterday's cut leaves the release without inventories,
	// and no later scheduled run backfills it. Redispatch the idempotent
	// chain for the last digest when its assets are incomplete; dry runs
	// and lookup failures skip green without dispatching.
	const reconcileSbom = () => {
		if (dryRun || !lastDigest || !sbomAssetsMissing(lastDigest)) return false;
		console.log(`${lastDigest} lacks SBOM assets; redispatching the SBOM chain`);
		output({
			tag_name: lastDigest, released: "true", prod_changed: "false",
			prev_tag: lastDigest,
		});
		return true;
	};
	const lastTarget = lastDigest ? tagTarget(lastDigest) : null;
	const prevTag = lastDigest || latestSemverReachable(sha);

	// Probe for today's tag once; tagTarget throws when the ref is missing,
	// which is the expected first-run path. Only the probe is guarded: a
	// later failure must exit red for a clean retry, never fall through.
	let existing = null;
	try {
		existing = tagTarget(digestTag);
	} catch {
		existing = null;
	}

	if (!dryRun && existing && !releaseExists(digestTag)) {
		// Today's digest tag exists but its Release is missing (a manual tag
		// or a failed create). Repair it before any skip: production may
		// already serve this exact SHA (quiet-day shape) yet the Release is
		// still owed. Notes range from the previous digest, never the tag
		// against itself. Dry runs skip the repair (no `gh` calls offline).
		pendingDigest();
		const repairPrev =
			latestDigestTagBefore(digestTag) || latestSemverReachable(existing);
		const opts = loadOptions("release-please-config.json");
		const range = repairPrev ? `${repairPrev}..${existing}` : existing;
		const commits = gitLog(range)
			.split("\x1e")
			.map((r) => r.trim())
			.filter(Boolean)
			.map((r) => {
				const [h, s, b] = r.split("\x1f");
				return parseCommit(h, s, b);
			})
			.filter(Boolean);
		const notes = renderNotes({
			tag: digestTag,
			prevTag: repairPrev || existing,
			date,
			commits,
			sections: opts.sections,
			repoUrl,
		});
		console.log(notes);
		const notesFile = `${process.env.RUNNER_TEMP || "/tmp"}/digest-notes-${digestTag}.md`;
		fs.writeFileSync(notesFile, notes);
		execFileSync(
			"gh",
			[
				"release",
				"create",
				digestTag,
				"--target",
				existing,
				"--title",
				digestTag,
				"--notes-file",
				notesFile,
				latestFlag,
			],
			{ stdio: "inherit" },
		);
		output({
			tag_name: digestTag,
			released: "true",
			prod_changed: "true",
			prev_tag: lastDigest || "",
		});
		return;
	}

	if (!dryRun && existing) {
		// Reconcile a lost SBOM dispatch even when production still matches
		// today's tag. Attaching inventories is idempotent (--clobber).
		output({
			tag_name: digestTag, released: "true", prod_changed: "false",
			prev_tag: lastDigest || "",
		});
		return;
	}

	if (lastTarget === sha) {
		if (reconcileSbom()) return;
		console.log(`${sha} already matches ${lastDigest}; quiet day, no new tag`);
		output({
			tag_name: "",
			released: "false",
			prod_changed: "false",
			prev_tag: lastDigest,
		});
		return;
	}
	if (lastTarget && isAncestor(sha, lastTarget)) {
		// Production serves an older commit than the last digest (a rollback
		// or redeploy of a prior build). It is never tagged, mirroring the
		// retired per-promote rollback guard; the next forward deploy digests.
		if (reconcileSbom()) return;
		console.log(
			`${sha} is older than ${lastDigest}; rollback serving, no new tag`,
		);
		output({
			tag_name: "",
			released: "false",
			prod_changed: "false",
			prev_tag: lastDigest,
		});
		return;
	}
	if (existing) {
		// Today's digest tag already exists with its Release (or a dry-run
		// preview of either). Tomorrow's digest covers any production move
		// since. Never a second tag for the same day.
		console.log(`${digestTag} already exists; no new tag today`);
		output({
			tag_name: "",
			released: "false",
			prod_changed: "false",
			prev_tag: lastDigest || "",
		});
		return;
	}

	if (!dryRun) pendingDigest();
	const opts = loadOptions("release-please-config.json");
	const range = prevTag ? `${prevTag}..${sha}` : sha;
	const commits = gitLog(range)
		.split("\x1e")
		.map((r) => r.trim())
		.filter(Boolean)
		.map((r) => {
			const [h, s, b] = r.split("\x1f");
			return parseCommit(h, s, b);
		})
		.filter(Boolean);
	const notes = renderNotes({
		tag: digestTag,
		prevTag: prevTag || sha,
		date,
		commits,
		sections: opts.sections,
		repoUrl,
	});
	console.log(notes);
	if (dryRun) {
		output({
			tag_name: "",
			released: "false",
			prod_changed: "true",
			prev_tag: lastDigest || "",
		});
		return;
	}
	if (!releaseExists(digestTag)) {
		const notesFile = `${process.env.RUNNER_TEMP || "/tmp"}/digest-notes-${digestTag}.md`;
		fs.writeFileSync(notesFile, notes);
		// Creates the tag on the live production commit when it does not exist
		// yet. Backdated cuts/repairs must not move Latest backward.
		execFileSync(
			"gh",
			[
				"release",
				"create",
				digestTag,
				"--target",
				sha,
				"--title",
				digestTag,
				"--notes-file",
				notesFile,
				latestFlag,
			],
			{ stdio: "inherit" },
		);
	}
	output({
		tag_name: digestTag,
		released: "true",
		prod_changed: "true",
		prev_tag: lastDigest || "",
	});
}

module.exports = {
	parseCommit,
	parseDigestTag,
	parseTag,
	renderNotes,
	compareVersions,
	loadOptions,
	LOG_MAX_BUFFER,
};

if (require.main === module) {
	try {
		main();
	} catch (err) {
		console.error(`::error title=daily-digest::${err.message}`);
		process.exit(1);
	}
}
