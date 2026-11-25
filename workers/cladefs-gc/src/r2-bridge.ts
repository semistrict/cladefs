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

export function createR2Bridge(bucket: R2Bucket): R2Bridge {
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
			let options: R2PutOptions | undefined;
			if (mode === "create") {
				options = { onlyIf: { etagDoesNotMatch: "*" } };
			} else if (mode === "update") {
				if (!etag) {
					throw makeCodedError("Precondition", `missing etag for update of ${key}`);
				}
				options = { onlyIf: { etagMatches: etag } };
			}
			const res = await bucket.put(key, bytes, options);
			if (!res) return null;
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
			return Date.now();
		},

		sleep(ms: number): Promise<void> {
			return new Promise((resolve) => setTimeout(resolve, ms));
		},
	};
}
