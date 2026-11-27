// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

export interface R2Meta {
	key: string;
	etag: string | null;
	version: string | null;
	size: number;
	modifiedMs: number;
}

export interface R2GetResult extends R2Meta {
	bytes: Uint8Array;
}

export interface R2Version {
	etag: string | null;
	version: string | null;
}

export interface R2Listing {
	prefixes: string[];
	objects: R2Meta[];
	next: string | null;
}

/**
 * Duck-typed bridge consumed by `crates/gc_wasm` implementing `bucket_cell::Bucket`
 * over a Cloudflare Worker `R2Bucket` binding.
 */
export interface R2Bridge {
	get(key: string): Promise<R2GetResult | null>;
	getRange(
		key: string,
		start: number | undefined,
		end: number | undefined,
		suffix: number | undefined,
	): Promise<Uint8Array | null>;
	head(key: string): Promise<R2Meta | null>;
	list(prefix: string): Promise<R2Meta[]>;
	listDelimited(prefix: string, after: string | undefined, max: number): Promise<R2Listing>;
	put(
		key: string,
		bytes: Uint8Array,
		mode: "overwrite" | "create" | "update",
		etag: string | undefined,
	): Promise<R2Version | null>;
	delete(key: string): Promise<void>;
	deleteBatch(keys: string[]): Promise<void>;
	now(): number;
	sleep(ms: number): Promise<void>;
}

/**
 * cladefs: the most the Worker's monotonic clock may run fast of R2's, in millionths, as
 * `bucket_cell::clock::DRIFT_PPM`.
 */
export const DRIFT_PPM = 500;

/**
 * cladefs: a bucket's clock as the collector reads it (`Bucket::now`): R2's, never the Worker's own,
 * as `crates/bucket_cell/src/clock.rs` has every host's (boards 8467, 8471, 14350). Collector leases
 * are measured by it, and the dates R2 gives objects (`R2Object.uploaded`) by R2's clock. The binding's
 * answers carry no `Date`, as native backends' do (`clock::Served`): the dates R2 gives what the
 * Worker writes stand in for them. It keeps their best lower bound: R2's time, as a write's answer
 * arrives, is at least the write's date, and since then it has gone on at least as fast as the
 * Worker's monotonic clock (`performance.now()`), less {@link DRIFT_PPM}. Never ahead of R2's, so a
 * lease it holds ends early, never late. 0 before any date: a lower bound still, which closes no
 * window; {@link probedBridge} writes before any pass reads it.
 */
export class BucketClock {
	#best: { ms: number; at: number } | null = null;

	/** A write R2 dated `dateMs`, answered at `at` by the monotonic clock: kept if it says more. */
	heard(dateMs: number, at: number = performance.now()): void {
		if (!Number.isFinite(dateMs) || dateMs < 0) return;
		const heard = { ms: dateMs, at };
		const best = this.#best;
		// Both as of the later's arrival, after which each goes on alike.
		const later = best === null ? at : Math.max(best.at, at);
		if (best === null || dated(best, later) < dated(heard, later)) {
			this.#best = heard;
		}
	}

	/** R2's time at `at`, at least: 0 before any date. */
	now(at: number = performance.now()): number {
		return this.#best === null ? 0 : Math.floor(dated(this.#best, at));
	}
}

/** R2's time at `then`, at least, as `date` says: its date, and the time since less the drift. */
function dated(date: { ms: number; at: number }, then: number): number {
	const since = Math.max(0, then - date.at);
	return date.ms + since - (since * DRIFT_PPM) / 1_000_000;
}

function toMeta(obj: R2Object): R2Meta {
	return {
		key: obj.key,
		etag: obj.etag ?? null,
		version: obj.version ?? null,
		size: obj.size,
		modifiedMs: obj.uploaded.getTime(),
	};
}

function makeCodedError(code: string, message: string): Error & { code: string } {
	const err = new Error(message) as Error & { code: string };
	err.code = code;
	return err;
}

/**
 * The bridge over `bucket`, its clock `clock` ({@link BucketClock}), fed the date R2 gives each
 * write.
 */
export function createR2Bridge(bucket: R2Bucket, clock: BucketClock = new BucketClock()): R2Bridge {
	return {
		async get(key: string): Promise<R2GetResult | null> {
			const obj = await bucket.get(key);
			if (!obj) return null;
			const bytes = new Uint8Array(await obj.arrayBuffer());
			return { ...toMeta(obj), bytes };
		},

		async getRange(
			key: string,
			start: number | undefined,
			end: number | undefined,
			suffix: number | undefined,
		): Promise<Uint8Array | null> {
			if (suffix !== undefined && suffix !== null) {
				if (suffix <= 0) {
					const meta = await bucket.head(key);
					return meta ? new Uint8Array(0) : null;
				}
				const obj = await bucket.get(key, { range: { suffix } });
				if (!obj) return null;
				if (!obj.body) return new Uint8Array(0);
				return new Uint8Array(await obj.arrayBuffer());
			}
			const from = start ?? 0;
			const to = end ?? from;
			const length = Math.max(0, to - from);
			if (length === 0) {
				const meta = await bucket.head(key);
				return meta ? new Uint8Array(0) : null;
			}
			const obj = await bucket.get(key, { range: { offset: from, length } });
			if (!obj) return null;
			if (!obj.body) return new Uint8Array(0);
			return new Uint8Array(await obj.arrayBuffer());
		},

		async head(key: string): Promise<R2Meta | null> {
			const obj = await bucket.head(key);
			return obj ? toMeta(obj) : null;
		},

		async list(prefix: string): Promise<R2Meta[]> {
			const out: R2Meta[] = [];
			let cursor: string | undefined;
			for (;;) {
				const page = await bucket.list({ prefix, cursor, limit: 1000 });
				for (const obj of page.objects) {
					out.push(toMeta(obj));
				}
				if (!page.truncated) break;
				cursor = page.cursor;
			}
			out.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
			return out;
		},

		async listDelimited(
			prefix: string,
			after: string | undefined,
			max: number,
		): Promise<R2Listing> {
			const page = await bucket.list({
				prefix,
				delimiter: "/",
				cursor: after ?? undefined,
				limit: Math.max(1, Math.min(1000, max)),
			});
			return {
				prefixes: [...page.delimitedPrefixes],
				objects: page.objects.map(toMeta),
				next: page.truncated ? page.cursor : null,
			};
		},

		async put(
			key: string,
			bytes: Uint8Array,
			mode: "overwrite" | "create" | "update",
			etag: string | undefined,
		): Promise<R2Version | null> {
			const res = await bucket.put(key, bytes, putOptions(key, mode, etag));
			if (!res) return null;
			clock.heard(res.uploaded.getTime());
			return { etag: res.etag ?? null, version: res.version ?? null };
		},

		async delete(key: string): Promise<void> {
			await bucket.delete(key);
		},

		async deleteBatch(keys: string[]): Promise<void> {
			for (let i = 0; i < keys.length; i += 1000) {
				await bucket.delete(keys.slice(i, i + 1000));
			}
		},

		now(): number {
			return clock.now();
		},

		sleep(ms: number): Promise<void> {
			return new Promise((resolve) => setTimeout(resolve, ms));
		},
	};
}

/**
 * cladefs: the condition of a write of `mode` (board 14350), as Cloudflare documents them for the
 * binding's `onlyIf` (R2 Workers API reference: `R2Conditional | Headers`, "All conditional headers
 * aside from `If-Range` are supported"; R2's conditional operations: "`*` matches any object", and
 * competing conditional writes to one key are serialized, checked as each commits):
 *
 * - create: `If-None-Match: *`, in the `Headers` form, which documents the wildcard; the object
 *   form documents `etagDoesNotMatch` as one etag compared, no wildcard (workerd sends both as the
 *   same wildcard, `buildSingleEtagArray`, but only the header is R2's documented create-only).
 * - update: `etagMatches` of the etag read, unquoted, as `R2Object.etag` gives it.
 */
export function putOptions(
	key: string,
	mode: "overwrite" | "create" | "update",
	etag: string | undefined,
): R2PutOptions | undefined {
	switch (mode) {
		case "overwrite":
			return undefined;
		case "create":
			return { onlyIf: new Headers({ "If-None-Match": "*" }) };
		case "update":
			if (!etag) {
				throw makeCodedError("Precondition", `missing etag for update of ${key}`);
			}
			return { onlyIf: { etagMatches: etag } };
	}
}

/**
 * cladefs: what a bucket's probe found wrong, as `bucket_cell::object_bucket::probe` refuses a store:
 * the collector's lease and its one deleter rest on create-only and versioned writes.
 */
function refused(what: string, found: unknown): Error {
	return makeCodedError(
		"Refused",
		`the store does not honour conditional writes: ${what} gave ${JSON.stringify(found)}`,
	);
}

/**
 * cladefs: probes `bucket` as native `cladefs-gc` probes its store before it writes anything
 * (`object_bucket::probe`): a create-only write where there is none, refused where there is one; an
 * update of another etag refused, of the object's taken; the object removed. Its writes' dates
 * start the clock ({@link BucketClock}), so that no lease is taken by a clock with no time. A
 * probe that finds the store wrong throws: no pass runs on it.
 */
export async function probe(bucket: R2Bucket, clock: BucketClock): Promise<void> {
	const nonce = crypto.getRandomValues(new Uint32Array(2));
	const key = `probe/worker-${nonce[0].toString(16)}${nonce[1].toString(16)}`;
	const bridge = createR2Bridge(bucket, clock);
	const created = await bridge.put(key, new TextEncoder().encode("probe"), "create", undefined);
	if (created?.etag == null) throw refused(`a create of ${key}`, created);
	const again = await bridge.put(key, new TextEncoder().encode("again"), "create", undefined);
	if (again !== null) throw refused("a create where an object is", again);
	const stale = await bridge.put(key, new TextEncoder().encode("stale"), "update", "cladefs-probe");
	if (stale !== null) throw refused("an update of another version", stale);
	const updated = await bridge.put(
		key,
		new TextEncoder().encode("updated"),
		"update",
		created.etag,
	);
	if (updated === null) throw refused("an update of the object's version", updated);
	await bridge.delete(key);
}

/** cladefs: each binding's clock, probed once an isolate ({@link probedBridge}). */
const probed = new WeakMap<R2Bucket, Promise<BucketClock>>();

/**
 * cladefs: the bridge over `bucket`, probed first ({@link probe}), once an isolate, as native
 * `cladefs-gc` probes once a process: its clock R2's, shared by the invocations that follow. A
 * failed probe is tried again by the next.
 */
export async function probedBridge(bucket: R2Bucket): Promise<R2Bridge> {
	let clock = probed.get(bucket);
	if (clock === undefined) {
		const made = new BucketClock();
		clock = probe(bucket, made).then(
			() => made,
			(error: unknown) => {
				probed.delete(bucket);
				throw error;
			},
		);
		probed.set(bucket, clock);
	}
	return createR2Bridge(bucket, await clock);
}
