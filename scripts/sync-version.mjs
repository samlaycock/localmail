// Copies the version changesets wrote to package.json into Cargo.toml and
// Cargo.lock, so `localmail --version` reports it too.
import { readFileSync, writeFileSync } from "node:fs";

const { version } = JSON.parse(readFileSync("package.json", "utf8"));

const manifest = readFileSync("Cargo.toml", "utf8");
const bumped = manifest.replace(/(\[package\][^[]*?\nversion = )"[^"]*"/, `$1"${version}"`);
if (bumped === manifest && !manifest.includes(`version = "${version}"`)) {
  throw new Error("Cargo.toml: no [package] version to update");
}
writeFileSync("Cargo.toml", bumped);

const lock = readFileSync("Cargo.lock", "utf8");
const lockBumped = lock.replace(/(name = "localmail"\nversion = )"[^"]*"/, `$1"${version}"`);
if (lockBumped === lock && !lock.includes(`name = "localmail"\nversion = "${version}"`)) {
  throw new Error("Cargo.lock: no localmail package entry to update");
}
writeFileSync("Cargo.lock", lockBumped);
console.log(`Cargo version set to ${version}`);
