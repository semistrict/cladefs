// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { bindings, defineConfig, triggers } from "cf/config";
import * as entrypoint from "./src/index.ts" with { type: "cf-worker" };

export default defineConfig({
	worker: {
		name: "cladefs-gc",
		workersDev: true,
		compatibilityDate: "2026-11-19",
		compatibilityFlags: ["nodejs_compat"],
		entrypoint,
		limits: {
			cpuMs: 300_000,
		},
		triggers: [
			triggers.scheduled({ schedule: "0 */6 * * *" }),
			triggers.queue({
				name: "cladefs-gc-cells",
				maxBatchSize: 1,
				maxConcurrency: 16,
				maxRetries: 3,
			}),
		],
		env: {
			BUCKET: bindings.r2({ name: "hotbed-cladefs" }),
			GC_QUEUE: bindings.queue({ name: "cladefs-gc-cells" }),
			GC_TOKEN: bindings.secret(),
			LEASE_MS: bindings.text("600000"),
		},
		observability: {
			enabled: true,
			logs: {
				enabled: true,
			},
			traces: {
				enabled: true,
			},
		},
	},
});
