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
//            digest-already-published) owes no tag but may owe SBOM assets
//            on the Latest release. Otherwise persist the owed-digest
//            outputs for the alert job and emit the publish request.
//   record   Read the publish step. A new Release dispatches the SBOM
//            chain; an existing one redispatches only when assets are
//            missing.
//
// Env: PROD_SHA (40-hex), GH_REPO, GITHUB_OUTPUT, DRY_RUN=true for a
// zero-write run; adapt reads SKIPPED, REASON, PLAN and PREV_TAG; record
// reads CREATED and TAG.

const { execFileSync } = require("node:child_process");
const fs = require("node:fs");

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
		const view = JSON.parse(execFileSync("gh", args, { encoding: "utf8" }));
		return view && typeof view.tagName === "string" ? view : null;
	} catch {
		return null;
	}
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

function request() {
	const sha = prodSha();
	const plan = {
		phase: "plan",
		input: { channel: "stable", policy: "live-digest", sha, operation: `daily-digest/${sha}` },
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

function adapt() {
	const sha = prodSha();
	const dry = dryRun();
	if (process.env.SKIPPED === "true") {
		const reason = process.env.REASON || "";
		if (!SKIP_REASONS.has(reason)) throw new Error(`unclassified skip reason ${JSON.stringify(reason)}`);
		const latest = dry ? null : releaseView("");
		console.log(`${sha} owes no digest today: ${reason}`);
		summary(`## Daily digest skipped: ${reason}\n\nLive production: \`${sha}\``);
		if (reason === "quiet-day") staleWarning(sha, latest, Date.now());
		// A red run after an earlier cut leaves its release without
		// inventories, and no later run backfills it. Redispatch the
		// idempotent chain for the Latest release when assets are missing.
		const owed = latest && assetsMissing(latest) ? latest.tagName : "";
		if (owed) console.log(`${owed} lacks SBOM assets; redispatching the SBOM chain`);
		output({
			publish: "false", tag_name: "", released: "false", prod_changed: "false",
			prev_tag: latest ? latest.tagName : "", sbom_tag: owed,
		});
		return;
	}
	let plan;
	try {
		plan = JSON.parse(process.env.PLAN || "");
	} catch {
		throw new Error("plan step returned neither a skip nor a JSON plan");
	}
	if (!plan || plan.sha !== sha || plan.policy !== "live-digest")
		throw new Error("plan does not bind the live production SHA under live-digest");
	// Persist the owed state before publication. On a failed publish these
	// outputs survive for the alert job; record overwrites released.
	// `repair` lets a retry finish a tag whose Release creation failed.
	const publish = { phase: "publish", plan, dryRun: dry, repair: true };
	output({
		publish: "true", tag_name: "", released: "false", prod_changed: "true",
		prev_tag: process.env.PREV_TAG || "", sbom_tag: "",
		publish_request: JSON.stringify(publish),
	});
}

function record() {
	const tag = (process.env.TAG || "").trim();
	if (!/^v[0-9]+\.[0-9]+\.[0-9]+$/.test(tag)) throw new Error("publish step returned no stable tag");
	if (dryRun()) {
		console.log(`dry run: would publish ${tag}`);
		output({ tag_name: "", released: "false", sbom_tag: "" });
		return;
	}
	let owed = tag;
	if (process.env.CREATED !== "true") {
		// An exact replay of an already-published digest: dispatch only when
		// its inventories are incomplete. A lookup failure stays green.
		const view = releaseView(tag);
		owed = view && assetsMissing(view) ? tag : "";
	}
	output({ tag_name: tag, released: "true", sbom_tag: owed });
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
