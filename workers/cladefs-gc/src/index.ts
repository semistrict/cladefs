// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { probedBridge } from "./r2-bridge.ts";
import {
	cellUsage,
	collectCell,
	dryRunCell,
	initSync,
	listCells,
	liveCell,
	readShreddedTenants,
} from "./wasm/gc_wasm.js";
import wasmModule from "./wasm/gc_wasm_bg.wasm";

let wasmReady = false;
function ensureWasm(): void {
	if (!wasmReady) {
		initSync({ module: wasmModule });
		wasmReady = true;
	}
}

export interface CellMessage {
	cell: string;
	sweepStartedMs: number;
	leaseMs: number;
	shredded: string[];
}

export interface CellEntry {
	tenant: string;
	cell: string;
	id: string;
}

export interface CollectResult {
	cell: string;
	deleted: number;
	deletedBytes: number;
	retired: number;
	skipped: number;
	removed: number;
	left?: "held" | "swept";
}

export interface DoomedEntry {
	key: string;
	kind: string;
	size: number;
}

export interface DryRunResult {
	cell: string;
	doomed: DoomedEntry[];
	doomedBytes: number;
	retired: string[];
	fenced: boolean;
}

export interface CellUsageResult {
	tenant: string;
	cell: string;
	measuredUnixMs: number | null;
	objects: number | null;
	bytes: number | null;
	kinds: Record<string, { objects: number; bytes: number }> | null;
}

export interface LiveCellResult {
	cell: string;
	live: boolean;
	originParent?: string;
	originAt?: string;
}

export interface Env {
	BUCKET: R2Bucket;
	GC_QUEUE: Queue<CellMessage>;
	GC_TOKEN?: string;
	LEASE_MS?: string;
}

function randomHolder(): string {
	const buf = new BigUint64Array(1);
	for (;;) {
		crypto.getRandomValues(buf);
		if (buf[0] !== 0n) return buf[0].toString();
	}
}

function parseLeaseMs(env: Env): number {
	const parsed = Number(env.LEASE_MS ?? "600000");
	return Number.isFinite(parsed) && parsed >= 0 ? parsed : 600_000;
}

function constantTimeEqual(a: string, b: string): boolean {
	const enc = new TextEncoder();
	const ba = enc.encode(a);
	const bb = enc.encode(b);
	if (ba.length !== bb.length) return false;
	let diff = 0;
	for (let i = 0; i < ba.length; i++) {
		diff |= ba[i] ^ bb[i];
	}
	return diff === 0;
}

function isAuthorized(request: Request, env: Env): boolean {
	const expected = env.GC_TOKEN;
	if (!expected) return false;
	const header = request.headers.get("authorization");
	if (!header?.startsWith("Bearer ")) return false;
	return constantTimeEqual(header.slice("Bearer ".length), expected);
}

function jsonResponse(body: unknown, status = 200): Response {
	return new Response(JSON.stringify(body), {
		status,
		headers: { "content-type": "application/json; charset=utf-8" },
	});
}

async function enqueueSweep(env: Env): Promise<{
	enqueued: number;
	sweepStartedMs: number;
	shreddedTenants: string[];
}> {
	ensureWasm();
	const bridge = await probedBridge(env.BUCKET);
	// The bucket's clock, as the passes' leases record when they ended (`Holding::since`).
	const sweepStartedMs = bridge.now();
	const leaseMs = parseLeaseMs(env);
	const [cellsJson, shreddedJson] = await Promise.all([
		listCells(bridge),
		readShreddedTenants(bridge),
	]);
	const cells = JSON.parse(cellsJson) as CellEntry[];
	const shredded = JSON.parse(shreddedJson) as string[];

	// Cloudflare Queues sendBatch accepts up to 100 messages per call.
	for (let i = 0; i < cells.length; i += 100) {
		const chunk = cells.slice(i, i + 100);
		await env.GC_QUEUE.sendBatch(
			chunk.map((entry) => ({
				body: {
					cell: entry.id,
					sweepStartedMs,
					leaseMs,
					shredded,
				},
			})),
		);
	}

	console.log(
		JSON.stringify({
			event: "sweep_enqueued",
			cells: cells.length,
			shreddedTenants: shredded.length,
			sweepStartedMs,
		}),
	);

	return { enqueued: cells.length, sweepStartedMs, shreddedTenants: shredded };
}

async function mapConcurrent<T, R>(
	items: T[],
	concurrency: number,
	fn: (item: T) => Promise<R>,
): Promise<R[]> {
	const results = new Array<R>(items.length);
	let next = 0;
	const workers = Array.from({ length: Math.min(concurrency, items.length) }, async () => {
		for (;;) {
			const i = next++;
			if (i >= items.length) break;
			results[i] = await fn(items[i]);
		}
	});
	await Promise.all(workers);
	return results;
}

export default {
	async scheduled(_controller: ScheduledController, env: Env): Promise<void> {
		await enqueueSweep(env);
	},

	async queue(batch: MessageBatch<CellMessage>, env: Env): Promise<void> {
		ensureWasm();
		const bridge = await probedBridge(env.BUCKET);
		for (const msg of batch.messages) {
			const started = Date.now();
			const { cell, sweepStartedMs, leaseMs, shredded } = msg.body;
			try {
				const raw = await collectCell(
					bridge,
					cell,
					leaseMs,
					sweepStartedMs,
					JSON.stringify(shredded),
					randomHolder(),
				);
				const result = JSON.parse(raw) as CollectResult;
				console.log(
					JSON.stringify({
						event: "cell_collected",
						...result,
						elapsedMs: Date.now() - started,
						attempt: msg.attempts,
					}),
				);
				msg.ack();
			} catch (err) {
				const error = err instanceof Error ? err.message : String(err);
				console.error(
					JSON.stringify({
						event: "cell_failed",
						cell,
						error,
						elapsedMs: Date.now() - started,
						attempt: msg.attempts,
					}),
				);
				msg.retry();
			}
		}
	},

	async fetch(request: Request, env: Env): Promise<Response> {
		const url = new URL(request.url);
		const path = url.pathname;

		if (request.method === "GET" && path === "/healthz") {
			return jsonResponse({ ok: true, worker: "cladefs-gc" });
		}

		if (!isAuthorized(request, env)) {
			return jsonResponse({ error: "unauthorized" }, 401);
		}

		ensureWasm();
		try {
			const bridge = await probedBridge(env.BUCKET);
			if (request.method === "POST" && path === "/sweep") {
				const result = await enqueueSweep(env);
				return jsonResponse(result);
			}

			if (request.method === "GET" && path === "/cells") {
				const cells = JSON.parse(await listCells(bridge)) as CellEntry[];
				return jsonResponse({ count: cells.length, cells });
			}

			const cellMatch = /^\/cell\/([^/]+)\/([^/]+)$/.exec(path);
			if (request.method === "POST" && cellMatch) {
				const cellId = `${cellMatch[1]}/${cellMatch[2]}`;
				const shreddedJson = await readShreddedTenants(bridge);
				const leaseMs = parseLeaseMs(env);
				const raw = await collectCell(
					bridge,
					cellId,
					leaseMs,
					undefined,
					shreddedJson,
					randomHolder(),
				);
				return jsonResponse(JSON.parse(raw) as CollectResult);
			}

			const dryRunCellMatch = /^\/dry-run\/([^/]+)\/([^/]+)$/.exec(path);
			if (request.method === "GET" && dryRunCellMatch) {
				const cellId = `${dryRunCellMatch[1]}/${dryRunCellMatch[2]}`;
				const shreddedJson = await readShreddedTenants(bridge);
				const raw = await dryRunCell(bridge, cellId, shreddedJson);
				return jsonResponse(JSON.parse(raw) as DryRunResult);
			}

			if (request.method === "GET" && path === "/dry-run") {
				const [cellsJson, shreddedJson] = await Promise.all([
					listCells(bridge),
					readShreddedTenants(bridge),
				]);
				const cells = JSON.parse(cellsJson) as CellEntry[];
				const results: DryRunResult[] = [];
				let totalDoomedObjects = 0;
				let totalDoomedBytes = 0;
				let totalRetired = 0;
				// Run sequentially to keep isolate RAM bounded under 128 MiB across all cells.
				for (const entry of cells) {
					const raw = await dryRunCell(bridge, entry.id, shreddedJson);
					const res = JSON.parse(raw) as DryRunResult;
					totalDoomedObjects += res.doomed.length;
					totalDoomedBytes += res.doomedBytes;
					totalRetired += res.retired.length;
					if (res.doomed.length > 0 || res.retired.length > 0 || res.fenced) {
						results.push(res);
					}
				}
				return jsonResponse({
					cellsScanned: cells.length,
					totalDoomedObjects,
					totalDoomedBytes,
					totalRetired,
					cellsWithGarbage: results,
				});
			}

			const usageCellMatch = /^\/usage\/([^/]+)\/([^/]+)$/.exec(path);
			if (request.method === "GET" && usageCellMatch) {
				const cellId = `${usageCellMatch[1]}/${usageCellMatch[2]}`;
				const raw = await cellUsage(bridge, cellId);
				return jsonResponse(JSON.parse(raw) as CellUsageResult);
			}

			if (request.method === "GET" && path === "/usage") {
				const cells = JSON.parse(await listCells(bridge)) as CellEntry[];
				const usages = await mapConcurrent(cells, 16, async (entry) => {
					const raw = await cellUsage(bridge, entry.id);
					return JSON.parse(raw) as CellUsageResult;
				});
				let totalObjects = 0;
				let totalBytes = 0;
				let measuredCells = 0;
				for (const u of usages) {
					if (u.objects !== null && u.bytes !== null) {
						measuredCells++;
						totalObjects += u.objects;
						totalBytes += u.bytes;
					}
				}
				return jsonResponse({
					cells: usages.length,
					measuredCells,
					totalObjects,
					totalBytes,
					usage: usages,
				});
			}

			const liveMatch = /^\/live\/([^/]+)\/([^/]+)$/.exec(path);
			if (request.method === "GET" && liveMatch) {
				const cellId = `${liveMatch[1]}/${liveMatch[2]}`;
				const raw = await liveCell(bridge, cellId);
				return jsonResponse(JSON.parse(raw) as LiveCellResult);
			}

			return jsonResponse({ error: "not found" }, 404);
		} catch (err) {
			const message = err instanceof Error ? err.message : String(err);
			return jsonResponse({ error: message }, 500);
		}
	},
} satisfies ExportedHandler<Env, CellMessage>;
