# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# fxfs-guest-python-sandbox's second workload (TASK-702.1): the same trips counted with pyarrow
# alone, no pandas and no marimo, so that a sandbox running it reads another part of the base than
# the notebook's does. Its answers, into ANSWERS (a JSON file, its keys sorted), are two of the
# notebook's.

import json
import os
from pathlib import Path

import pyarrow.compute as pc
import pyarrow.parquet as pq

data = Path(os.environ.get("TAXI_DATA", "/app/data"))
answers_path = Path(os.environ.get("ANSWERS", "/tmp/answers.json"))
trips = 0
trips_in_2025 = 0
for path in sorted(data.glob("green_tripdata_2025-*.parquet")):
    table = pq.read_table(path, columns=["lpep_pickup_datetime"])
    trips += table.num_rows
    in_2025 = pc.equal(pc.year(table["lpep_pickup_datetime"]), 2025)
    trips_in_2025 += table.filter(in_2025).num_rows
answers = {"trips": trips, "trips_in_2025": trips_in_2025}
answers_path.write_text(json.dumps(answers, sort_keys=True) + "\n")
