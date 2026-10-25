#!/usr/bin/env node
// Builds the DBX Store candidate file from a published plugin release.
//
// The store's own sync script (dbx-store/scripts/sync-release-candidate.mjs)
// derives candidates/<plugin-id>.json from the release's release-candidates.json
// plus .dbx-store.json. This mirrors that logic so a first submission can be
// prepared and reviewed locally without guessing at any field.
//
//   node store/make-candidate.mjs \
//     --repo bobocha214/dbx-plugin-modbus \
//     --tag v0.1.0 \
//     --release-candidates ./release-candidates.json \
//     --metadata ./.dbx-store.json \
//     --output ./store/candidates/io.github.bobocha214.modbus.json
//
// --release-candidates also accepts the asset's HTTPS URL.

import { readFile, writeFile, mkdir } from "node:fs/promises";
import path from "node:path";

const options = parseOptions(process.argv.slice(2));
const repository = required(options, "repo");
const tag = required(options, "tag");
const candidatesSource = required(options, "release-candidates");
const metadataPath = options.metadata || ".dbx-store.json";
const output = required(options, "output");

assert(/^[^/]+\/[A-Za-z0-9._-]+$/.test(repository), `Invalid repository '${repository}'`);
assert(tag && !tag.includes(".."), "Invalid release tag");

const release = await readJson(candidatesSource);
const identity = release.plugin;
assert(identity && typeof identity === "object" && !Array.isArray(identity),
  "release-candidates.json has no plugin identity block");
for (const field of ["id", "publisher", "version"]) {
  assert(typeof identity[field] === "string" && identity[field].length > 0,
    `Missing plugin identity field '${field}'`);
}
assert(Array.isArray(release.artifacts) && release.artifacts.length > 0,
  "Release contains no plugin artifacts");

const metadata = await readOptionalJson(metadataPath) ?? {};
const allowedMetadata = new Set([
  "name", "description", "icon", "tags", "permissions", "source",
  "homepage", "license", "releaseNotes", "localizations",
]);
for (const key of Object.keys(metadata)) {
  assert(allowedMetadata.has(key), `Unsupported store metadata field '${key}'`);
}
if (metadata.icon && !/^[A-Za-z][A-Za-z0-9+.-]*:\/\//.test(metadata.icon)) {
  metadata.icon = `https://raw.githubusercontent.com/${repository}/${encodeURIComponent(tag)}/${metadata.icon.replace(/^\/+/, "")}`;
}

const candidate = {
  schemaVersion: 1,
  id: identity.id,
  publisher: identity.publisher,
  version: identity.version,
  ...metadata,
  targets: release.artifacts.map((artifact) => ({
    target: artifact.target,
    url: releaseAssetUrl(repository, tag, artifact.url),
    sha256: artifact.sha256,
    size: artifact.size,
  })).sort((left, right) => left.target.localeCompare(right.target)),
};

// The manifest permissions inside the built package are authoritative; the
// hand-maintained .dbx-store.json copy only matters when the release tooling
// omits the field. Keep both in sync anyway.
if (Array.isArray(identity.permissions) && identity.permissions.length > 0) {
  candidate.permissions = [...new Set(identity.permissions)].sort();
}

candidate.name ??= identity.name;
candidate.description ??= identity.description ?? "";
candidate.source ??= `https://github.com/${repository}/tree/${encodeURIComponent(tag)}`;
candidate.homepage ??= `https://github.com/${repository}`;
assert(typeof candidate.name === "string" && candidate.name.length > 0,
  "New plugins require store metadata 'name'");
assert(typeof candidate.license === "string" && candidate.license.length > 0,
  "New plugins require store metadata 'license'");

assertTargets(candidate.targets);

await mkdir(path.dirname(output), { recursive: true });
await writeFile(output, `${JSON.stringify(candidate, null, 2)}\n`);
console.log(`Wrote ${output}: ${candidate.id}@${candidate.version}, ${candidate.targets.length} target(s)`);
for (const target of candidate.targets) {
  console.log(`  ${target.target.padEnd(14)} ${String(target.size).padStart(9)} B  ${target.sha256.slice(0, 12)}…`);
}

function assertTargets(targets) {
  const seen = new Set();
  for (const entry of targets) {
    assert(/^[a-z0-9-]{1,64}$/.test(entry.target), `Invalid target name '${entry.target}'`);
    assert(!seen.has(entry.target), `Duplicate target '${entry.target}'`);
    seen.add(entry.target);
    assert(/^[A-Fa-f0-9]{64}$/.test(entry.sha256), `Invalid sha256 for ${entry.target}`);
    assert(Number.isSafeInteger(entry.size) && entry.size > 0, `Invalid size for ${entry.target}`);
    const url = new URL(entry.url);
    assert(url.protocol === "https:", `Target URL for ${entry.target} must be HTTPS`);
    assert(!url.pathname.startsWith("/t8y2/dbx-store/"),
      `Target URL for ${entry.target} must not point at dbx-store releases`);
  }
}

function releaseAssetUrl(sourceRepository, releaseTag, artifactUrl) {
  assert(typeof artifactUrl === "string" && artifactUrl.length > 0, "Artifact URL is required");
  const parsed = new URL(artifactUrl, "https://artifact.invalid/");
  const fileName = path.posix.basename(parsed.pathname);
  assert(/^[A-Za-z0-9._-]+\.dbxp$/.test(fileName), `Invalid candidate artifact '${artifactUrl}'`);
  return `https://github.com/${sourceRepository}/releases/download/${encodeURIComponent(releaseTag)}/${encodeURIComponent(fileName)}`;
}

function parseOptions(args) {
  const parsed = {};
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (!argument.startsWith("--")) throw new Error(`Unexpected argument '${argument}'`);
    const key = argument.slice(2);
    const value = args[index + 1];
    if (!value || value.startsWith("--")) throw new Error(`Missing value for '${argument}'`);
    parsed[key] = value;
    index += 1;
  }
  return parsed;
}

function required(parsed, key) {
  if (!parsed[key]) throw new Error(`Missing required option '--${key}'`);
  return parsed[key];
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

async function readJson(source) {
  if (/^https:\/\//.test(source)) {
    const response = await fetch(source, { headers: { accept: "application/json" } });
    if (!response.ok) throw new Error(`Failed to fetch ${source}: HTTP ${response.status}`);
    return response.json();
  }
  return JSON.parse(await readFile(source, "utf8"));
}

async function readOptionalJson(file) {
  try {
    return JSON.parse(await readFile(file, "utf8"));
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
}
