// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { env, SELF } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { createR2Bridge } from "../src/r2-bridge.ts";
import worker, { type CellMessage, type CollectResult, type DryRunResult } from "../src/index.ts";

const AUTH = { authorization: "Bearer test-gc-token" };

describe("R2Bridge primitives", () => {
	it("supports conditional create/update, ranged reads, delimited listing, and batch delete", async () => {
		const bridge = createR2Bridge(env.BUCKET);
		const payload = new TextEncoder().encode("0123456789abcdef");

		// Create succeeds once, then returns null (AlreadyExists).
		const v1 = await bridge.put("test/obj", payload, "create", undefined);
		expect(v1?.etag).toBeTruthy();
		const dup = await bridge.put("test/obj", payload, "create", undefined);
		expect(dup).toBeNull();

		// Conditional update with wrong etag returns null (Precondition); right etag succeeds.
		const bad = await bridge.put("test/obj", payload, "update", "wrong-etag");
		expect(bad).toBeNull();
		const v2 = await bridge.put(
			"test/obj",
			new TextEncoder().encode("fedcba9876543210"),
			"update",
			v1!.etag!,
		);
		expect(v2?.etag).toBeTruthy();

		// Bounded and suffix ranged reads.
		const bounded = await bridge.getRange("test/obj", 2, 6, undefined);
		expect(new TextDecoder().decode(bounded!)).toBe("dcba");
		const suffix = await bridge.getRange("test/obj", undefined, undefined, 4);
		expect(new TextDecoder().decode(suffix!)).toBe("3210");
		const missing = await bridge.getRange("test/missing", 0, 4, undefined);
		expect(missing).toBeNull();

		// Delimited listing across prefixes and direct objects.
		await bridge.put("delim/a/1", new Uint8Array([1]), "overwrite", undefined);
		await bridge.put("delim/b/2", new Uint8Array([2]), "overwrite", undefined);
		await bridge.put("delim/root", new Uint8Array([3]), "overwrite", undefined);
		const listing = await bridge.listDelimited("delim/", undefined, 100);
		expect(listing.prefixes.sort()).toEqual(["delim/a/", "delim/b/"]);
		expect(listing.objects.map((o) => o.key)).toEqual(["delim/root"]);

		// Batch delete removes all keys.
		await bridge.deleteBatch(["test/obj", "delim/a/1", "delim/b/2", "delim/root"]);
		expect(await bridge.head("test/obj")).toBeNull();
		expect((await bridge.list("delim/")).length).toBe(0);
	});
});

describe("cladefs-gc Worker + WASM", () => {
	it("serves /healthz without auth and rejects unauthorized requests", async () => {
		const health = await SELF.fetch("http://example.com/healthz");
		expect(health.status).toBe(200);
		expect(await health.json()).toEqual({ ok: true, worker: "cladefs-gc" });

		const unauth = await SELF.fetch("http://example.com/cells");
		expect(unauth.status).toBe(401);
	});

	it("lists cells, dry-runs, and collects stray unreferenced objects in a cell via WASM", async () => {
		// Seed a stray segment under cell 0/1 (no head -> dead cell, stray object is garbage).
		// cladefs keys use 13-char zero-padded base32 IDs (`Padded`).
		const cellKey =
			"tenants/0000000000000/cells/0000000000001/segments/0000000000001.0000000000000";
		await env.BUCKET.put(cellKey, new Uint8Array([1, 2, 3, 4]));

		// /cells discovers 0/1 via delimiter listing.
		const listRes = await SELF.fetch("http://example.com/cells", { headers: AUTH });
		expect(listRes.status).toBe(200);
		const listed = (await listRes.json()) as { count: number; cells: { id: string }[] };
		expect(listed.cells.map((c) => c.id)).toContain("0/1");

		// /dry-run/0/1 reports the stray segment as doomed without deleting it.
		const dryRes = await SELF.fetch("http://example.com/dry-run/0/1", { headers: AUTH });
		expect(dryRes.status).toBe(200);
		const dry = (await dryRes.json()) as DryRunResult;
		expect(dry.cell).toBe("0/1");
		expect(dry.doomed.map((d) => d.key)).toContain(cellKey);
		expect(dry.fenced).toBe(false);
		expect(await env.BUCKET.head(cellKey)).not.toBeNull();

		// Queue consumer collects cell 0/1 and deletes the stray segment.
		let acked = false;
		let retried = false;
		const msg: Message<CellMessage> = {
			id: "msg-1",
			timestamp: new Date(),
			attempts: 1,
			body: {
				cell: "0/1",
				sweepStartedMs: Date.now(),
				leaseMs: 60_000,
				shredded: [],
			},
			ack() {
				acked = true;
			},
			retry() {
				retried = true;
			},
		};
		await worker.queue(
			{
				queue: "cladefs-gc-cells",
				messages: [msg],
				ackAll() {},
				retryAll() {},
			} as unknown as MessageBatch<CellMessage>,
			env,
		);
		expect(acked).toBe(true);
		expect(retried).toBe(false);
		expect(await env.BUCKET.head(cellKey)).toBeNull();

		// Dead cell 0/1 with no head has its entire prefix cleaned up (including gc/lease).
		const remaining01 = await env.BUCKET.list({
			prefix: "tenants/0000000000000/cells/0000000000001/",
		});
		expect(remaining01.objects.length).toBe(0);

		// Seed cell 0/2 with an object GC preserves (`Kind::Other`) so its lease & usage persist:
		// pass 1 records the listing digest & usage; pass 2 skips unchanged (`skipped === 1`).
		const keptKey = "tenants/0000000000000/cells/0000000000002/note";
		await env.BUCKET.put(keptKey, new Uint8Array([9, 8, 7]));

		const pass1Res = await SELF.fetch("http://example.com/cell/0/2", {
			method: "POST",
			headers: AUTH,
		});
		expect(pass1Res.status).toBe(200);
		const pass1 = (await pass1Res.json()) as CollectResult;
		expect(pass1.deleted).toBe(0);
		expect(pass1.skipped).toBe(0);

		const pass2Res = await SELF.fetch("http://example.com/cell/0/2", {
			method: "POST",
			headers: AUTH,
		});
		expect(pass2Res.status).toBe(200);
		const pass2 = (await pass2Res.json()) as CollectResult;
		expect(pass2.deleted).toBe(0);
		expect(pass2.skipped).toBe(1);

		// /usage/0/2 reads the collector lease recorded by the WASM pass.
		const usageRes = await SELF.fetch("http://example.com/usage/0/2", { headers: AUTH });
		expect(usageRes.status).toBe(200);
		const usage = (await usageRes.json()) as {
			cell: string;
			measuredUnixMs: number | null;
			objects: number | null;
		};
		expect(usage.cell).toBe("0/2");
		expect(usage.measuredUnixMs).not.toBeNull();
		expect(usage.objects).toBe(2); // note + gc/lease
	});
});
