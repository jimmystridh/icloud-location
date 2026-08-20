from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any


REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
FIXTURES = REPOSITORY_ROOT / "tests" / "fixtures"


def load_fixture(relative_path: str) -> dict[str, Any]:
    with (FIXTURES / relative_path).open(encoding="utf-8") as fixture_file:
        return json.load(fixture_file)


def reference_distance_interval(case: dict[str, Any]) -> int:
    if not case["direction_known"]:
        return 150

    distance_km = case["distance_km"]
    if distance_km < 2 and case["went_beyond_three_km"]:
        return 15
    if distance_km < 2:
        return 60
    if distance_km < 3.5:
        return 90
    if distance_km < 5:
        return 120
    if distance_km < 8:
        return 180
    if distance_km < 12:
        return 300
    if distance_km < 20:
        return 600
    if distance_km < 40:
        return 900
    if distance_km > 150:
        return 3600
    return round((distance_km * 0.621371) * 0.5) * 60


def haversine_meters(
    latitude_a: float,
    longitude_a: float,
    latitude_b: float,
    longitude_b: float,
) -> float:
    earth_radius_meters = 6_371_008.8
    latitude_delta = math.radians(latitude_b - latitude_a)
    longitude_delta = math.radians(longitude_b - longitude_a)
    latitude_a_radians = math.radians(latitude_a)
    latitude_b_radians = math.radians(latitude_b)
    haversine = (
        math.sin(latitude_delta / 2) ** 2
        + math.cos(latitude_a_radians)
        * math.cos(latitude_b_radians)
        * math.sin(longitude_delta / 2) ** 2
    )
    return earth_radius_meters * 2 * math.atan2(
        math.sqrt(haversine), math.sqrt(1 - haversine)
    )


def reference_selected_zone(
    zones: list[dict[str, Any]], case: dict[str, Any]
) -> str | None:
    accuracy_allowance = int(case["horizontal_accuracy_meters"] / 2)
    candidates = []
    for zone in zones:
        if zone["passive"]:
            continue
        distance = haversine_meters(
            case["latitude"],
            case["longitude"],
            zone["latitude"],
            zone["longitude"],
        )
        if distance <= zone["radius_meters"] + accuracy_allowance:
            candidates.append(zone)
    if not candidates:
        return None
    return min(candidates, key=lambda zone: zone["radius_meters"])["id"]


def segment_value(segment: dict[str, Any], camel: str, snake: str) -> float:
    if camel in segment:
        return float(segment[camel])
    return float(segment[snake])


def validate_waze_fixture() -> None:
    fixture = load_fixture("waze/route_response.json")
    segments = fixture["response"]["results"]
    realtime_seconds = sum(
        segment_value(segment, "crossTime", "cross_time") for segment in segments
    )
    historical_seconds = sum(
        segment_value(
            segment, "crossTimeWithoutRealTime", "cross_time_without_real_time"
        )
        for segment in segments
    )
    distance_meters = sum(segment["length"] for segment in segments)
    expected = fixture["expected"]
    assert math.isclose(realtime_seconds / 60, expected["realtime_time_minutes"])
    assert math.isclose(
        historical_seconds / 60, expected["historical_time_minutes"]
    )
    assert math.isclose(distance_meters / 1000, expected["distance_km"])


def reference_retry_interval(error_count: int) -> int:
    if error_count <= 1:
        return 5
    ranges = load_fixture("tracking/retry_intervals.json")["ranges"]
    return next(
        entry["seconds"]
        for entry in reversed(ranges)
        if entry["minimum_count"] <= error_count
    )


def validate_sanitization() -> None:
    forbidden_fragments = ("@stridh", "jimmy-apple", "x-apple-session-token")
    for fixture_path in FIXTURES.rglob("*.json"):
        contents = fixture_path.read_text(encoding="utf-8").lower()
        for fragment in forbidden_fragments:
            assert fragment not in contents, f"{fragment!r} found in {fixture_path}"


def main() -> None:
    intervals = load_fixture("tracking/interval_cases.json")
    for case in intervals["cases"]:
        assert reference_distance_interval(case) == case["expected_seconds"], case["name"]

    zone_fixture = load_fixture("zones/zone_cases.json")
    for case in zone_fixture["cases"]:
        assert (
            reference_selected_zone(zone_fixture["zones"], case)
            == case["expected_zone"]
        ), case["name"]

    validate_waze_fixture()
    retry_fixture = load_fixture("tracking/retry_intervals.json")
    for error_count in range(0, 30):
        expected = next(
            entry["seconds"]
            for entry in reversed(retry_fixture["ranges"])
            if entry["minimum_count"] <= error_count
        )
        assert reference_retry_interval(error_count) == expected
    validate_sanitization()
    print(
        f"validated {len(intervals['cases'])} interval cases, "
        f"{len(zone_fixture['cases'])} zone cases, Waze aggregation, and fixture sanitization"
    )


if __name__ == "__main__":
    main()
