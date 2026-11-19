# Copyright 2026 The cladefs Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# fxfs-guest-python-sandbox's notebook (TASK-665): a year of New York's green-taxi trips, read
# with pandas from Parquet, and a few questions about them, answered into ANSWERS (a JSON file),
# which the case checks against what the same questions gave outside the VM.

import marimo

app = marimo.App()


@app.cell
def _():
    import json
    import os
    from pathlib import Path

    import pandas as pd

    data = Path(os.environ.get("TAXI_DATA", "/app/data"))
    answers_path = Path(os.environ.get("ANSWERS", "/tmp/answers.json"))
    return answers_path, data, json, pd


@app.cell
def _(data, pd):
    trips = pd.concat(
        [pd.read_parquet(path) for path in sorted(data.glob("green_tripdata_2025-*.parquet"))],
        ignore_index=True,
    )
    zones = pd.read_csv(data / "taxi_zone_lookup.csv")
    in_2025 = trips[trips["lpep_pickup_datetime"].dt.year == 2025]
    return in_2025, trips, zones


@app.cell
def _(in_2025, zones):
    pickups = in_2025.merge(
        zones, left_on="PULocationID", right_on="LocationID", how="left"
    )
    by_borough = pickups["Borough"].value_counts()
    by_zone = pickups["Zone"].value_counts()
    return by_borough, by_zone


@app.cell
def _(in_2025):
    by_month = in_2025["lpep_pickup_datetime"].dt.month.value_counts()
    by_hour = in_2025["lpep_pickup_datetime"].dt.hour.value_counts()
    card = in_2025[(in_2025["payment_type"] == 1) & (in_2025["fare_amount"] > 0)]
    tip_pct = (card["tip_amount"] / card["fare_amount"]).mean() * 100
    return by_hour, by_month, tip_pct


@app.cell
def _(
    answers_path,
    by_borough,
    by_hour,
    by_month,
    by_zone,
    in_2025,
    json,
    tip_pct,
    trips,
):
    answers = {
        "trips": int(len(trips)),
        "trips_in_2025": int(len(in_2025)),
        "busiest_month": int(by_month.idxmax()),
        "busiest_hour": int(by_hour.idxmax()),
        "busiest_borough": str(by_borough.idxmax()),
        "top_pickup_zone": str(by_zone.idxmax()),
        "median_miles": round(float(in_2025["trip_distance"].median()), 2),
        "card_tip_percent": round(float(tip_pct), 2),
    }
    answers_path.write_text(json.dumps(answers, sort_keys=True) + "\n")
    answers
    return


if __name__ == "__main__":
    app.run()
