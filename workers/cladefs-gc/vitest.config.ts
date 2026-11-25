// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vite-plus";

export default defineConfig({
	plugins: [
		cloudflareTest({
			config: true,
			miniflare: {
				bindings: {
					GC_TOKEN: "test-gc-token",
					LEASE_MS: "600000",
				},
			},
		}),
	],
});
