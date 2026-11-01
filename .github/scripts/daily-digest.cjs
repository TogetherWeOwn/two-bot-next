#!/usr/bin/env node
"use strict";

// Daily digest adapter for the shared CalVer releaser (docs/releases.md).
// The shared action (TogetherWeOwn/.github actions/calver, `live-digest`
// policy) allocates vYYYY.MDD.P, renders notes and publishes the tag and
// Release. This adapter keeps the bot-specific behaviour around it:
//
//   request  Build the read-only plan request for the live production SHA.
//            The operation is bound to the SHA, so a retry for the same
//            build recovers its frozen plan instead of allocating again.
//   adapt    Read the plan step. A classified skip (quiet-day, rollback,
//            digest-already-published) and a replay of a SHA whose Release
//            already exists owe no tag. Today's CalVer tag left without its
//            Release (a failed publish before production moved again) is
//            finished from its own frozen plan. Otherwise persist the
//            owed-digest outputs for the alert job and emit the publish
//            request. Every non-dry path reconciles the Latest release's
//            SBOM assets.
//   record   Read the publish step. A new Release dispatches the SBOM
//            chain; an existing one redispatches only when assets are
//            missing.
//
// Env: PROD_SHA (40-hex), GH_REPO, GITHUB_OUTPUT, DRY_RUN=true for a
// zero-write run (no gh calls besides the shared action's own reads); adapt
// reads SKIPPED, REASON, PLAN and PREV_TAG; record reads CREATED and TAG.
// sbom_tags is a space-separated list of release tags owed an SBOM dispatch.

const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const SKIP_REASONS = new Set(["quiet-day", "rollback", "digest-already-published"]);
// release.yml uploads these to every digest; sbom.yml names the two inventories.
const SBOM_ASSETS = [
	"rust-workspace.cdx.json",
	"container-image.cdx.json",
	"SHA256SUMS",
	"source-sha.txt",
	"image-id.txt",
];
// A quiet streak longer than this while main moves ahead is a stalled
// production, not a quiet day (2026-10-23..31: guard skips reported green).
const STALE_DAYS = 3;
const DAY_MS = 24 * 60 * 60 * 1000;
const STABLE_TAG = /^v[0-9]+\.[0-9]+\.[0-9]+$/;
const ANNOTATION = "calver-plan-v1\n";

function output(values) {
	const text = Object.entries(values)
		.map(([k, v]) => `${k}=${v}\n`)
		.join("");
	if (process.env.GITHUB_OUTPUT)
		fs.appendFileSync(process.env.GITHUB_OUTPUT, text);
	process.stdout.write(text);
}

function summary(text) {
	if (process.env.GITHUB_STEP_SUMMARY)
		fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${text}\n`);
}

function prodSha() {
	const sha = (process.env.PROD_SHA || "").trim().toLowerCase();
	if (!/^[0-9a-f]{40}$/.test(sha))
		throw new Error("PROD_SHA must be a full 40-character commit SHA");
	return sha;
}

function dryRun() {
	return process.env.DRY_RUN === "true";
}

// One release lookup; null when it fails. A quiet day must never turn red
// or spam dispatches on a lookup error. No tag selects the Latest release.
function releaseView(tag) {
	const args = ["release", "view", ...(tag ? [tag] : []), "--json", "tagName,assets,publishedAt"];
	try {
		const view = ghJson(args);
		return view && typeof view.tagName === "string" && STABLE_TAG.test(view.tagName) ? view : null;
	} catch {
		return null;
	}
}

function ghJson(args) {
	return JSON.parse(execFileSync("gh", args, { encoding: "utf8" }));
}

function assetsMissing(view) {
	const names = new Set((view.assets || []).map((a) => a && a.name).filter(Boolean));
	return SBOM_ASSETS.some((asset) => !names.has(asset));
}

// Commits main is ahead of the live SHA; null when the lookup fails.
function aheadOfLive(sha) {
	try {
		const ahead = execFileSync(
			"gh",
			["api", `repos/${process.env.GH_REPO}/compare/${sha}...main`, "--jq", ".ahead_by"],
			{ encoding: "utf8" },
		).trim();
		return /^[0-9]+$/.test(ahead) ? Number(ahead) : null;
	} catch {
		return null;
	}
}

// The bot's note sections live in release-please-config.json (Common
// Changelog headings, `security` under Fixed); the shared releaser
// validates and freezes them into the plan. Fail closed on a missing or
// malformed list instead of silently falling back to built-in headings.
function changelogSections() {
	const config = JSON.parse(
		fs.readFileSync(path.join(__dirname, "..", "..", "release-please-config.json"), "utf8"),
	);
	const sections = config.packages?.["."]?.["changelog-sections"];
	if (!Array.isArray(sections) || sections.length === 0)
		throw new Error("release-please-config.json must list changelog-sections for the digest notes");
	return sections;
}

function request() {
	const sha = prodSha();
	const plan = {
		phase: "plan",
		input: {
			channel: "stable", policy: "live-digest", sha,
			operation: `daily-digest/${sha}`, sections: changelogSections(),
		},
	};
	output({ request: JSON.stringify(plan) });
}

// Warn, never fail: the stall belongs to deploy-production, and a red
// digest would page for a day that owes no tag.
function staleWarning(sha, latest, now) {
	const published = Date.parse((latest && latest.publishedAt) || "");
	if (!Number.isFinite(published) || now - published <= STALE_DAYS * DAY_MS) return;
	const ahead = aheadOfLive(sha);
	if (!ahead) return;
	const days = Math.floor((now - published) / DAY_MS);
	const text =
		`Production still serves ${sha.slice(0, 7)} (${latest.tagName}, ${days} days old) ` +
		`while main is ${ahead} commits ahead; check deploy-production for skipped runs`;
	console.log(`::warning title=daily-digest stalled production::${text}`);
	summary(`### Stalled production\n\n${text}.`);
}

// Today's CalVer tag whose Release is missing, as its own frozen plan read
// from the tag annotation the shared releaser wrote; null when there is
// none or a lookup fails.
function unfinishedToday(now) {
	const d = new Date(now);
	const prefix = `v${d.getUTCFullYear()}.${d.getUTCMonth() + 1}${String(d.getUTCDate()).padStart(2, "0")}.`;
	try {
		const refs = ghJson(["api", `repos/${process.env.GH_REPO}/git/matching-refs/tags/${prefix}`]);
		for (const ref of Array.isArray(refs) ? refs : []) {
			const tag = String(ref && ref.ref).replace(/^refs\/tags\//, "");
			if (!STABLE_TAG.test(tag) || ref.object?.type !== "tag" || releaseView(tag)) continue;
			const message = ghJson(["api", `repos/${process.env.GH_REPO}/git/tags/${ref.object.sha}`]).message;
			if (typeof message !== "string" || !message.startsWith(ANNOTATION)) continue;
			const { publicationEvidence, ...plan } = JSON.parse(message.slice(ANNOTATION.length));
			if (plan.tag === tag && plan.policy === "live-digest") return plan;
		}
	} catch {
		return null;
	}
	return null;
}

function noDigest(reason, latest, owed) {
	summary(`## Daily digest skipped: ${reason}\n\nLive production: \`${prodSha()}\``);
	output({
		publish: "false", tag_name: "", released: "false", prod_changed: "false",
		prev_tag: latest ? latest.tagName : "", sbom_tags: [...new Set(owed)].join(" "),
	});
}

// Persist the owed state before publication. On a failed publish these
// outputs survive for the alert job; record overwrites released.
// `repair` lets a retry finish a tag whose Release creation failed.
function owe(plan, prevTag, owed) {
	const publish = { phase: "publish", plan, dryRun: dryRun(), repair: true };
	output({
		publish: "true", tag_name: "", released: "false", prod_changed: "true",
		prev_tag: prevTag, sbom_tags: owed.join(" "),
		publish_request: JSON.stringify(publish),
	});
}

function adapt() {
	const sha = prodSha();
	const dry = dryRun();
	const now = Date.now();
	// A red run after an earlier cut leaves its release without inventories,
	// and no later run backfills it. Every path redispatches the idempotent
	// chain for the Latest release when its assets are missing.
	const latest = dry ? null : releaseView("");
	const owed = latest && assetsMissing(latest) ? [latest.tagName] : [];
	if (owed.length) console.log(`${owed[0]} lacks SBOM assets; redispatching the SBOM chain`);
	if (process.env.SKIPPED === "true") {
		const reason = process.env.REASON || "";
		if (!SKIP_REASONS.has(reason)) throw new Error(`unclassified skip reason ${JSON.stringify(reason)}`);
		const unfinished = !dry && reason === "digest-already-published" ? unfinishedToday(now) : null;
		if (unfinished) {
			console.log(`${unfinished.tag} has no Release; finishing it from its frozen plan`);
			owe(unfinished, unfinished.previousStableTag || "", owed);
			return;
		}
		console.log(`${sha} owes no digest today: ${reason}`);
		if (reason === "quiet-day") staleWarning(sha, latest, now);
		noDigest(reason, latest, owed);
		return;
	}
	let plan;
	try {
		plan = JSON.parse(process.env.PLAN || "");
	} catch {
		throw new Error("plan step returned neither a skip nor a JSON plan");
	}
	if (!plan || plan.sha !== sha || plan.policy !== "live-digest" || !STABLE_TAG.test(plan.tag))
		throw new Error("plan does not bind the live production SHA under live-digest");
	// The shared planner recovers a SHA's prior plan before its quiet-day and
	// rollback checks. When that Release already exists, production has not
	// moved to anything new: owe nothing, and never re-publish (a transient
	// publish error would page for a digest that exists).
	const own = dry ? null : releaseView(plan.tag);
	if (own) {
		console.log(`${sha} is already published as ${plan.tag}; no new digest`);
		if (latest && latest.tagName === plan.tag) staleWarning(sha, latest, now);
		if (assetsMissing(own)) owed.push(plan.tag);
		noDigest("already-published", latest, owed);
		return;
	}
	owe(plan, process.env.PREV_TAG || "", owed);
}

function record() {
	const tag = (process.env.TAG || "").trim();
	if (!STABLE_TAG.test(tag)) throw new Error("publish step returned no stable tag");
	if (dryRun()) {
		console.log(`dry run: would publish ${tag}`);
		output({ tag_name: "", released: "false", sbom_tags: "" });
		return;
	}
	let owed = tag;
	if (process.env.CREATED !== "true") {
		// A repaired or replayed digest: dispatch only when its inventories
		// are incomplete. A lookup failure stays green.
		const view = releaseView(tag);
		owed = view && assetsMissing(view) ? tag : "";
	}
	output({ tag_name: tag, released: "true", sbom_tags: owed });
}

const MODES = { request, adapt, record };

module.exports = { SBOM_ASSETS, STALE_DAYS, assetsMissing, MODES };

if (require.main === module) {
	try {
		const mode = MODES[process.argv[2]];
		if (!mode) throw new Error("usage: daily-digest.cjs request|adapt|record");
		mode();
	} catch (err) {
		console.error(`::error title=daily-digest::${err.message}`);
		process.exit(1);
	}
}
